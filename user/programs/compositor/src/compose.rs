//! 整帧合成：桌面背景加按栈序排列的窗口。每帧整屏重画，所以前后缓冲的陈旧内容不影响结果。

use canvas::{Color, Image, Rect, Surface};
use classic_theme::{SystemColor, Theme};
use linux_uapi::drm::DumbBuffer;

use crate::wayland::Content;
pub(crate) mod gpu;

/// 一个待绘制的 surface：左上角物理坐标、内容与放大倍数。
pub struct Drawable<'a> {
    pub x: i32,
    pub y: i32,
    pub content: &'a Content,
    /// 内容相对其自身 scale 放大到输出 scale 所需的整数倍数。
    pub factor: i32,
}

/// 把桌面与窗口画进 `target`，最后交给 `overlay` 在最上层绘制（Alt+Tab 面板）。
///
/// # Returns
///
/// `false` 表示缓冲尺寸与行距不能构成绘制面（不应发生，由 DRM 保证）；此时没有绘制。
pub fn compose(
    target: &mut DumbBuffer,
    theme: &Theme,
    windows: &[Drawable<'_>],
    inversions: &[Rect],
    overlay: impl FnOnce(Surface<'_>),
) -> bool {
    let (width, height, stride) = (
        target.width() as i32,
        target.height() as i32,
        target.stride(),
    );
    let Some(mut surface) = Surface::new(target.pixels_mut(), width, height, stride) else {
        return false;
    };
    let background = theme.color(SystemColor::Background);
    surface.fill_rect(
        Rect::new(0, 0, width, height),
        Color::rgb(background.red, background.green, background.blue),
    );
    for window in windows {
        let image = Image {
            pixels: &window.content.pixels,
            width: window.content.width as i32,
            height: window.content.height as i32,
            stride: window.content.width,
        };
        surface.blit(image, window.x, window.y, window.factor);
    }
    for rect in inversions {
        surface.invert(*rect);
    }
    overlay(surface);
    true
}
