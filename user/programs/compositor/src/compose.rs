//! 输出合成：CPU/GPU 共用损伤、缓冲年龄修复与从上向下的遮挡裁剪。

use canvas::{Color, Image, Rect, Surface};
use classic_theme::{SystemColor, Theme};
use linux_uapi::drm::DumbBuffer;

use crate::wayland::Content;
pub(crate) mod damage;
pub(crate) mod gpu;

/// 一个已提交 surface 在输出上的物理目标矩形；CPU/GPU 消费同一几何。
pub struct Drawable<'a> {
    pub rect: Rect,
    pub content: &'a Content,
    /// 真实连接/surface 身份；命中测试的临时几何不参与输出历史。
    pub identity: Option<(u64, u32)>,
    pub damage: &'a [Rect],
    pub opaque: bool,
}
impl<'a> Drawable<'a> {
    pub fn surface(x: i32, y: i32, content: &'a Content, output_scale: i32) -> Self {
        let (width, height) = content.output_size(output_scale);
        let position = |origin: i32, offset: i64| {
            i64::from(origin)
                .saturating_add(offset.saturating_mul(i64::from(output_scale)))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        let (x, y) = (position(x, content.offset.0), position(y, content.offset.1));
        Self {
            rect: Rect::new(x, y, width, height),
            content,
            identity: None,
            damage: &[],
            opaque: false,
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
pub(super) fn compose(
    target: &mut DumbBuffer,
    theme: &Theme,
    windows: &[Drawable<'_>],
    frame: &damage::Frame,
    overlay: impl FnMut(Surface<'_>, (i32, i32)),
) -> std::io::Result<bool> {
    let (width, height, stride) = (
        target.width() as i32,
        target.height() as i32,
        target.stride(),
    );
    let Some(surface) = Surface::new(target.pixels_mut(), width, height, stride) else {
        return Ok(false);
    };
    paint(surface, theme, windows, frame, overlay)?;
    Ok(true)
}

fn paint(
    mut surface: Surface<'_>,
    theme: &Theme,
    windows: &[Drawable<'_>],
    frame: &damage::Frame,
    mut overlay: impl FnMut(Surface<'_>, (i32, i32)),
) -> std::io::Result<()> {
    let background = theme.color(SystemColor::Background);
    for rect in frame.background.rects() {
        surface.fill_rect(
            *rect,
            Color::rgb(background.red, background.green, background.blue),
        );
    }
    for (window, visible) in windows.iter().zip(&frame.windows) {
        if visible.is_empty() {
            continue;
        }
        let image = Image {
            pixels: window.content.pixels()?,
            width: window.content.width as i32,
            height: window.content.height as i32,
            stride: window.content.width,
        };
        for rect in visible.rects() {
            surface.set_clip(*rect);
            surface.blit_sampled(image, window.rect, window.content.source);
        }
    }
    for rect in frame.overlay.rects() {
        surface.set_clip(*rect);
        // 借用同一目标的独立重借用；每块区域不相交，透明叠加只执行一次。
        overlay(surface.reborrow(), (0, 0));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
