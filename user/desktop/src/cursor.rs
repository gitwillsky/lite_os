//! XP 风格箭头指针光标：运行时从 rootfs `/usr/share/liteos/cursor.lc1` 读入
//! 1bpp 轮廓（黑）+ 填充（白）两张位图（资产由 `scripts/generate_cursor.py`
//! 生成，随镜像分发，不内嵌进二进制），作为合成最后一层参与 damage 重画
//! （不做背景保存 / 恢复）。
//!
//! 文件布局（小端）：8B magic `LCR1\0\0\0\x01`、u32 width、u32 height，随后
//! 依次是轮廓与填充两张 1bpp 位图，各 `height * ceil(width/8)` 字节，每字节
//! MSB 对应行内最左像素。尺寸固定 32x32（[`rect_at`] 的 damage 契约，与
//! `chrome::SCALE` 2× 的物理大小一致），热点在 `(0, 0)`（箭头尖）。
//! 文件缺失或校验失败返回 `None`（启动失败）——光标是桌面唯一指针反馈，
//! 没有可降级的替代路径。

use crate::{
    ffi,
    scanout::{Frame, Rect},
};

pub const WIDTH: i32 = 32;
pub const HEIGHT: i32 = 32;

/// rootfs 中的光标路径（NUL 结尾）。
const PATH: &[u8] = b"/usr/share/liteos/cursor.lc1\0";
const MAGIC: &[u8; 8] = b"LCR1\0\0\0\x01";
/// 资产头字节数（magic + width + height）。
const HEADER: usize = 16;
/// 每张位图的字节数（32 行 × 4 字节）。
const BITMAP_SIZE: usize = (HEIGHT as usize) * (WIDTH as usize / 8);

const BLACK: u32 = 0;
const WHITE: u32 = 0x00ff_ffff;

/// checked 解析后的光标资产（进程生命周期持有映射，退出时由内核回收）。
pub struct Cursor {
    bytes: &'static [u8],
}

impl Cursor {
    /// 从 rootfs 读入光标并校验：magic、尺寸恰为 32x32、文件长度恰好对齐。
    /// 任一失败返回 `None`（文件缺失、截断或内容损坏）。
    pub fn open() -> Option<Self> {
        let (pointer, size) = ffi::read_file(PATH)?;
        // SAFETY: pointer/size 来自 read_file 的匿名映射，进程生命周期内有效。
        let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, size) };
        let valid = bytes.len() == HEADER + 2 * BITMAP_SIZE
            && bytes.get(..8) == Some(MAGIC.as_slice())
            && read_u32(bytes, 8) == Some(WIDTH as u32)
            && read_u32(bytes, 12) == Some(HEIGHT as u32);
        if !valid {
            // 校验失败不返回资产：释放映射（desktop 启动失败会退避重试，不能
            // 每次重试泄漏一份映射）。
            // SAFETY: 映射由本函数持有，此后不再访问。
            unsafe { ffi::munmap(pointer, size) };
            return None;
        }
        Some(Self { bytes })
    }

    /// 把光图画进 scanout，只写 `clip` 覆盖的像素。
    pub fn paint(&self, frame: &mut Frame, x: i32, y: i32, clip: Rect) {
        let area = rect_at(x, y).intersect(clip).intersect(Rect::new(
            0,
            0,
            frame.width() as i32,
            frame.height() as i32,
        ));
        if area.is_empty() {
            return;
        }
        for pixel_y in area.y1..area.y2 {
            let local_y = (pixel_y - y) as usize;
            let row = frame.row(pixel_y as usize);
            for pixel_x in area.x1..area.x2 {
                let local_x = (pixel_x - x) as usize;
                let byte = |offset: usize| {
                    self.bytes[offset + local_y * (WIDTH as usize / 8) + local_x / 8]
                };
                let bit = 0x80 >> (local_x & 7);
                if byte(HEADER) & bit != 0 {
                    row[pixel_x as usize] = BLACK;
                } else if byte(HEADER + BITMAP_SIZE) & bit != 0 {
                    row[pixel_x as usize] = WHITE;
                }
            }
        }
    }
}

/// 光标在 `(x, y)` 时覆盖的屏幕矩形（光标移动时新旧各记一次 damage）。
pub fn rect_at(x: i32, y: i32) -> Rect {
    Rect::new(x, y, x + WIDTH, y + HEIGHT)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}
