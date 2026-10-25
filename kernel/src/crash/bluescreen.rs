//! 内核蓝屏：panic 时由内核直接绘制的停机画面（Windows 2000 风格：蓝底、白色等宽文字）。
//!
//! 画面只依赖内嵌的 8x16 ASCII 字体与 [`PanicFramebuffer`]，不需要文件系统、分配器或 DRM；
//! 它画到 adapter 为 panic 专设的 scanout resource 上，因此 compositor 正占用屏幕时也能显示。

use core::{
    fmt::{self, Write},
    panic::PanicInfo,
};

use super::font::{FIRST, GLYPHS, HEIGHT, LAST, WIDTH};
use crate::drivers::{DisplayError, PanicFramebuffer};

/// XRGB8888 小端字节序：B、G、R、X。
const BACKGROUND: [u8; 4] = [0xaa, 0x00, 0x00, 0xff];
const FOREGROUND: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
/// 屏幕四周留白（像素）。
const MARGIN: usize = 16;
/// 蓝屏上展示的回溯帧数；完整回溯在崩溃记录里。
const SHOWN_FRAMES: usize = 8;

/// 蓝屏内容的来源。
pub(super) struct Report<'a> {
    pub(super) info: &'a PanicInfo<'a>,
    pub(super) cpu: usize,
    pub(super) uptime_seconds: u64,
    pub(super) frames: &'a [usize],
    /// 崩溃记录区是否可用；不可用时不要提示用户“已保存”。
    pub(super) record_saved: bool,
}

/// 按字符网格往帧缓冲画字的 `fmt::Write`；超出屏幕的内容静默丢弃。
struct Screen<'a> {
    display: &'a dyn PanicFramebuffer,
    pitch: usize,
    columns: usize,
    rows: usize,
    column: usize,
    row: usize,
}

impl<'a> Screen<'a> {
    /// 整屏涂成蓝色并返回光标位于左上角的画布；`None` 表示屏幕小于一个字符。
    fn clear(display: &'a dyn PanicFramebuffer) -> Option<Self> {
        let mode = display.mode();
        let (width, height) = (mode.width as usize, mode.height as usize);
        let columns = width.checked_sub(2 * MARGIN)? / WIDTH;
        let rows = height.checked_sub(2 * MARGIN)? / HEIGHT;
        if columns == 0 || rows == 0 {
            return None;
        }
        let mut line = [0u8; 4096];
        for pixel in line.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&BACKGROUND);
        }
        let total = mode.pitch as usize * height;
        let mut offset = 0;
        while offset < total {
            let count = line.len().min(total - offset);
            display.write(offset, &line[..count]).ok()?;
            offset += count;
        }
        Some(Self {
            display,
            pitch: mode.pitch as usize,
            columns,
            rows,
            column: 0,
            row: 0,
        })
    }

    fn newline(&mut self) {
        self.column = 0;
        self.row += 1;
    }

    fn draw(&mut self, byte: u8) -> Result<(), DisplayError> {
        let glyph = &GLYPHS[usize::from(byte.clamp(FIRST, LAST) - FIRST)];
        for (y, bits) in glyph.iter().enumerate() {
            let mut pixels = [0u8; WIDTH * 4];
            for (x, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let lit = bits & (0x80 >> x) != 0;
                pixel.copy_from_slice(if lit { &FOREGROUND } else { &BACKGROUND });
            }
            let offset =
                (MARGIN + self.row * HEIGHT + y) * self.pitch + (MARGIN + self.column * WIDTH) * 4;
            self.display.write(offset, &pixels)?;
        }
        Ok(())
    }
}

impl fmt::Write for Screen<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.newline();
                continue;
            }
            if self.column == self.columns {
                self.newline();
            }
            if self.row >= self.rows {
                return Ok(());
            }
            // 单个字形写失败（越界）只影响该字符，蓝屏仍要尽量显示其余内容。
            let _ = self.draw(if byte.is_ascii_graphic() || byte == b' ' {
                byte
            } else {
                b'?'
            });
            self.column += 1;
        }
        Ok(())
    }
}

/// 绘制并呈现蓝屏；任何一步失败都静默放弃，串口与崩溃记录仍然有效。
pub(super) fn show(display: &dyn PanicFramebuffer, report: &Report<'_>) {
    let Some(mut screen) = Screen::clear(display) else {
        return;
    };
    let _ = writeln!(screen, "*** STOP: KERNEL_PANIC\n");
    let _ = writeln!(screen, "{}", report.info.message());
    if let Some(location) = report.info.location() {
        let _ = writeln!(screen, "at {}:{}", location.file(), location.line());
    }
    let _ = writeln!(
        screen,
        "\nCPU {}    Uptime {} s    LiteOS {}\n",
        report.cpu,
        report.uptime_seconds,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(screen, "Call stack (resolve with addr2line -e kernel):");
    for address in report.frames.iter().take(SHOWN_FRAMES) {
        let _ = writeln!(screen, "  {address:#018x}");
    }
    let _ = writeln!(screen);
    if report.record_saved {
        let _ = writeln!(
            screen,
            "A crash record was saved to reserved memory and the serial console."
        );
        let _ = writeln!(
            screen,
            "After a warm restart read it from /proc/last_panic."
        );
    } else {
        let _ = writeln!(
            screen,
            "The crash record could not be saved to reserved memory."
        );
    }
    let _ = writeln!(
        screen,
        "\nContact your system administrator or technical support group.\nRestart your computer to continue."
    );
    let _ = display.present();
}
