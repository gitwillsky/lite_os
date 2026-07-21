//! 关机画面与关机动作：开始菜单 `关机` 项确认后由事件循环调用一次。
//!
//! 先整屏画 Luna 蓝渐变（#00309C→#0058E6，顶→底）与中央 bold28 白字
//! "Windows 正在关机..."，damage 全屏 `DIRTYFB` 提交一次；然后 fork 子进程
//! `execve("/bin/shutdown", ["shutdown", "-h", "now"])`（init 收尸并关机）。
//! 返回后事件循环停止响应输入，保持画面等 init 关机。

use crate::{
    scanout::{Rect, Scanout},
    uifont::{Face, UiFont},
};

const GRADIENT_TOP: u32 = 0x0000_309c;
const GRADIENT_BOTTOM: u32 = 0x0000_58e6;
const TEXT: u32 = 0x00ff_ffff;
const MESSAGE: &str = "Windows 正在关机...";

/// 画关机画面并提交，然后 fork + execve `/bin/shutdown`（fork / execve 失败
/// 静默忽略：画面保持，等外部 reset）。
pub fn enter(scanout: &mut Scanout, font: &UiFont) {
    let mode = scanout.mode();
    let (width, height) = (mode.width as i32, mode.height as i32);
    let screen = Rect::new(0, 0, width, height);
    {
        let mut frame = scanout.frame();
        for y in 0..height {
            frame.row(y as usize).fill(gradient(y, height));
        }
        // bold28 文字块垂直居中、水平居中。
        let face = Face::Bold28;
        let text_width = font.measure(face, MESSAGE);
        let baseline = (height - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
        font.draw(
            &mut frame,
            face,
            TEXT,
            ((width - text_width) / 2, baseline),
            MESSAGE,
            screen,
        );
    }
    scanout.present(&[screen]);
    let _ = std::process::Command::new("/bin/shutdown")
        .args(["-h", "now"])
        .spawn();
}

/// 垂直渐变：`y` ∈ [0, height) 在 top→bottom 间线性插值。
fn gradient(y: i32, height: i32) -> u32 {
    let mix = |top: u32, bottom: u32| {
        (top * (height - 1 - y) as u32 + bottom * y as u32) / (height.max(1) - 1).max(1) as u32
    };
    let red = mix(GRADIENT_TOP >> 16 & 0xff, GRADIENT_BOTTOM >> 16 & 0xff);
    let green = mix(GRADIENT_TOP >> 8 & 0xff, GRADIENT_BOTTOM >> 8 & 0xff);
    let blue = mix(GRADIENT_TOP & 0xff, GRADIENT_BOTTOM & 0xff);
    red << 16 | green << 8 | blue
}
