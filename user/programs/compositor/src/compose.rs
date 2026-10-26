//! 整帧合成：桌面背景加按栈序排列的窗口。每帧整屏重画，所以前后缓冲的陈旧内容不影响结果。

use canvas::{Color, Image, Rect, Surface};
use classic_theme::{SystemColor, Theme};
use linux_uapi::drm::DumbBuffer;

use crate::wayland::Content;
pub(crate) mod gpu;

/// 一个已提交 surface 在输出上的物理目标矩形；CPU/GPU 消费同一几何。
pub struct Drawable<'a> {
    pub rect: Rect,
    pub content: &'a Content,
}
impl<'a> Drawable<'a> {
    pub fn surface(x: i32, y: i32, content: &'a Content, factor: i32) -> Self {
        Self {
            rect: Rect::new(
                x,
                y,
                (content.width as i32).saturating_mul(factor),
                (content.height as i32).saturating_mul(factor),
            ),
            content,
        }
    }
    /// 把本项从整个窗口的 source 坐标系映射到目标矩形；窗口与所有子 surface 共用同一变换。
    pub fn transformed(mut self, source: Rect, target: Rect) -> Self {
        let map = |value: i32, origin: i32, extent: i32, dest: i32, size: i32| {
            dest + ((i64::from(value) - i64::from(origin)) * i64::from(size) / i64::from(extent))
                as i32
        };
        let rect = self.rect;
        let left = map(rect.x, source.x, source.width, target.x, target.width);
        let top = map(rect.y, source.y, source.height, target.y, target.height);
        self.rect = Rect::new(
            left,
            top,
            (map(rect.right(), source.x, source.width, target.x, target.width) - left).max(0),
            (map(
                rect.bottom(),
                source.y,
                source.height,
                target.y,
                target.height,
            ) - top)
                .max(0),
        );
        self
    }
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
        surface.blit_scaled(image, window.rect);
    }
    overlay(surface);
    true
}
