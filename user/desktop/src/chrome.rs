//! SSD 窗口装饰：布局纯函数 + 裁剪绘制。
//!
//! 布局：标题栏 32px（横跨整个外框顶部），左右下边框 2px，关闭按钮为标题栏
//! 右侧 32x32 方块（内画白色 X）。配色用 Luna 蓝底白字（焦点 / 非焦点两档），
//! 第三期与视觉还原一起精修。

use crate::{
    atlas::{self, Atlas},
    scanout::{Frame, Rect},
};

/// 标题栏高度（px）。
pub const TITLE_HEIGHT: i32 = 32;
/// 左 / 右 / 下边框宽度（px）。
pub const BORDER: i32 = 2;
/// 关闭按钮边长（px），位于标题栏右侧。
pub const CLOSE_SIZE: i32 = 32;

const TITLE_FOCUSED: u32 = 0x0000_58e6;
const TITLE_UNFOCUSED: u32 = 0x007a_9adb;
const TEXT: u32 = 0x00ff_ffff;
/// 标题文字左缘相对外框的缩进。
const TEXT_INDENT: i32 = 8;

/// 装饰布局（相对窗口外框原点）。
#[derive(Clone, Copy)]
pub struct Layout {
    /// 外框宽度（含边框）。
    pub outer_width: i32,
    /// 外框高度（含标题栏与边框）。
    pub outer_height: i32,
    /// 标题栏矩形（含关闭按钮区域）。
    pub title_bar: Rect,
    /// 关闭按钮矩形。
    pub close_button: Rect,
    /// 内容区原点相对外框原点的偏移。
    pub content_origin: (i32, i32),
}

/// 由内容尺寸计算装饰布局；`decorated = false` 时外框即内容本身。
pub fn layout(content_width: i32, content_height: i32, decorated: bool) -> Layout {
    if !decorated {
        return Layout {
            outer_width: content_width,
            outer_height: content_height,
            title_bar: Rect::new(0, 0, 0, 0),
            close_button: Rect::new(0, 0, 0, 0),
            content_origin: (0, 0),
        };
    }
    let outer_width = content_width + 2 * BORDER;
    let outer_height = TITLE_HEIGHT + content_height + BORDER;
    Layout {
        outer_width,
        outer_height,
        title_bar: Rect::new(0, 0, outer_width, TITLE_HEIGHT),
        close_button: Rect::new(outer_width - BORDER - CLOSE_SIZE, 0, outer_width - BORDER, CLOSE_SIZE),
        content_origin: (BORDER, TITLE_HEIGHT),
    }
}

/// 把装饰（标题栏 + 边框 + 关闭按钮 + 标题文字）画进 scanout，
/// 只写 `clip` 覆盖的像素。`outer` 为窗口外框的屏幕坐标原点。
pub fn paint(
    frame: &mut Frame,
    atlas: &Atlas,
    outer: (i32, i32),
    window_layout: &Layout,
    title: &[u8],
    focused: bool,
    clip: Rect,
) {
    let title_color = if focused {
        TITLE_FOCUSED
    } else {
        TITLE_UNFOCUSED
    };
    let screen = Rect::new(0, 0, frame.width() as i32, frame.height() as i32);
    let clip = clip.intersect(screen);
    if clip.is_empty() {
        return;
    }
    let outer_rect = Rect::new(
        outer.0,
        outer.1,
        outer.0 + window_layout.outer_width,
        outer.1 + window_layout.outer_height,
    );
    // 标题栏（含关闭按钮底色）。
    fill(frame, shift(window_layout.title_bar, outer).intersect(clip), title_color);
    // 左 / 右 / 下边框。
    let left = Rect::new(outer.0, outer.1 + TITLE_HEIGHT, outer.0 + BORDER, outer_rect.y2);
    let right = Rect::new(outer_rect.x2 - BORDER, outer.1 + TITLE_HEIGHT, outer_rect.x2, outer_rect.y2);
    let bottom = Rect::new(outer.0, outer_rect.y2 - BORDER, outer_rect.x2, outer_rect.y2);
    fill(frame, left.intersect(clip), title_color);
    fill(frame, right.intersect(clip), title_color);
    fill(frame, bottom.intersect(clip), title_color);
    let button = shift(window_layout.close_button, outer);
    paint_close(frame, button, button.intersect(clip));
    paint_title(
        frame,
        atlas,
        outer,
        window_layout,
        title,
        title_color,
        clip,
    );
}

/// 关闭按钮：在按钮矩形内画白色 X（两条 2px 斜线），只写 `clip` 内像素。
fn paint_close(frame: &mut Frame, button: Rect, clip: Rect) {
    if clip.is_empty() {
        return;
    }
    const MARGIN: i32 = 10;
    let span = CLOSE_SIZE - 2 * MARGIN - 1;
    for y in clip.y1..clip.y2 {
        let row = frame.row(y as usize);
        let local_y = y - button.y1 - MARGIN;
        for x in clip.x1..clip.x2 {
            let local_x = x - button.x1 - MARGIN;
            let on_diagonal = (local_x - local_y).abs() <= 1
                || (local_x + local_y - span).abs() <= 1;
            if (0..=span).contains(&local_x) && (0..=span).contains(&local_y) && on_diagonal {
                row[x as usize] = TEXT;
            }
        }
    }
}

/// 标题文字：16x32 cell，alpha blend 白字到标题栏底色上，裁到 `clip`。
fn paint_title(
    frame: &mut Frame,
    atlas: &Atlas,
    outer: (i32, i32),
    window_layout: &Layout,
    title: &[u8],
    title_color: u32,
    clip: Rect,
) {
    let metrics = atlas.metrics();
    let cell_width = metrics.width() as i32;
    let cell_height = metrics.height() as i32;
    let text_right = outer.0 + window_layout.close_button.x1;
    let Ok(text) = core::str::from_utf8(title) else {
        return;
    };
    let mut pen_x = outer.0 + TEXT_INDENT;
    for character in text.chars() {
        if pen_x + cell_width > text_right {
            break;
        }
        let cell = Rect::new(pen_x, outer.1, pen_x + cell_width, outer.1 + cell_height);
        let area = cell.intersect(clip);
        if !area.is_empty() {
            let glyph = atlas.glyph(character as u32, false);
            for y in area.y1..area.y2 {
                let row = frame.row(y as usize);
                for x in area.x1..area.x2 {
                    let alpha = glyph
                        [((y - outer.1) as usize) * metrics.width() + (x - pen_x) as usize];
                    if alpha != 0 {
                        row[x as usize] = atlas::blend(title_color, TEXT, alpha);
                    }
                }
            }
        }
        pen_x += cell_width;
    }
}

/// 把 `area`（屏幕坐标，调用方保证已裁到屏幕内）填为 `color`。
fn fill(frame: &mut Frame, area: Rect, color: u32) {
    if area.is_empty() {
        return;
    }
    for y in area.y1..area.y2 {
        frame.row(y as usize)[area.x1 as usize..area.x2 as usize].fill(color);
    }
}

fn shift(rect: Rect, origin: (i32, i32)) -> Rect {
    Rect::new(
        rect.x1 + origin.0,
        rect.y1 + origin.1,
        rect.x2 + origin.0,
        rect.y2 + origin.1,
    )
}
