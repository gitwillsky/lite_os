//! SSD 窗口装饰：布局纯函数 + 裁剪绘制（第三期 Luna 视觉）。
//!
//! 布局：标题栏 64px（横跨整个外框顶部），左右下边框 4px；标题栏右侧从右到左
//! 依次为关闭（X）、最大化 / 还原（□/❐）、最小化（—）三个 64x64 命中单元，
//! 间距 4px。所有 UI 度量按 [`SCALE`] 统一 2× 缩放（常量注释标注 1× 基准值）。
//!
//! Luna 视觉：标题栏垂直渐变（焦点 #0058E6→#3D95FF，非焦点 #7697E7→#9DB9EB），
//! 顶部两角圆角（半径 12px，圆角像素不画即露出下层）；边框取标题栏渐变底色；
//! 按钮为命中单元内居中的 48x48 圆角方块（关闭=红渐变白 X，最大化 / 最小化=
//! 蓝渐变白字形，按下态压暗）；标题文字 uifont bold32 白色。

use crate::{
    scanout::{Frame, Rect},
    uifont::{Face, UiFont},
};

/// 桌面统一 HiDPI 缩放因子：所有 UI 度量（chrome / taskbar / startmenu /
/// cursor / pointer 命中带与尺寸下限等）以 1× 基准值乘 `SCALE` 得到，本常量
/// 是缩放因子的唯一 owner。显示 mode 与指针坐标不缩放（屏幕绝对坐标）。
pub const SCALE: i32 = 2;

/// 标题栏高度（px，1× 基准 32）。
pub const TITLE_HEIGHT: i32 = 32 * SCALE;
/// 左 / 右 / 下边框宽度（px，1× 基准 2）。
pub const BORDER: i32 = 2 * SCALE;
/// 标题栏按钮命中单元边长（px，1× 基准 32），位于标题栏右侧。
pub const BUTTON_SIZE: i32 = 32 * SCALE;
/// 相邻按钮间距（px，1× 基准 2）。
pub const BUTTON_GAP: i32 = 2 * SCALE;

/// 按钮可视方块边长（px，1× 基准 24），在命中单元内居中。
const BUTTON_VISUAL: i32 = 24 * SCALE;
/// 标题栏顶部圆角半径（px，1× 基准 6）。
const CORNER_RADIUS: i32 = 6 * SCALE;
/// 按钮可视方块圆角半径（px，1× 基准 4）。
const BUTTON_RADIUS: i32 = 4 * SCALE;

const TITLE_FOCUSED_TOP: u32 = 0x0000_58e6;
const TITLE_FOCUSED_BOTTOM: u32 = 0x003d_95ff;
const TITLE_UNFOCUSED_TOP: u32 = 0x0076_97e7;
const TITLE_UNFOCUSED_BOTTOM: u32 = 0x009d_b9eb;
const CLOSE_TOP: u32 = 0x00e0_8080;
const CLOSE_BOTTOM: u32 = 0x00c0_5050;
const BUTTON_TOP: u32 = 0x005a_9ae0;
const BUTTON_BOTTOM: u32 = 0x002f_6ac0;
const TEXT: u32 = 0x00ff_ffff;
/// 标题文字左缘相对外框的缩进（px，1× 基准 8）。
const TEXT_INDENT: i32 = 8 * SCALE;
/// 按钮白色图形相对可视方块边缘的缩进（px，1× 基准 7）。
const GLYPH_MARGIN: i32 = 7 * SCALE;

/// 标题栏按钮种类（布局与绘制共用；hit-test 的 `Region` 由其映射而来）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// 关闭（X）。
    Close,
    /// 最大化 / 还原（□/❐）。
    Maximize,
    /// 最小化（—）。
    Minimize,
}

/// 装饰布局（相对窗口外框原点）。
#[derive(Clone, Copy)]
pub struct Layout {
    /// 外框宽度（含边框）。
    pub outer_width: i32,
    /// 外框高度（含标题栏与边框）。
    pub outer_height: i32,
    /// 标题栏矩形（含按钮区域）。
    pub title_bar: Rect,
    /// 关闭按钮矩形。
    pub close_button: Rect,
    /// 最大化 / 还原按钮矩形。
    pub maximize_button: Rect,
    /// 最小化按钮矩形。
    pub minimize_button: Rect,
    /// 内容区原点相对外框原点的偏移。
    pub content_origin: (i32, i32),
}

/// 由内容尺寸计算外框尺寸（`decorated = false` 时外框即内容本身）。
pub fn outer_size(content_width: i32, content_height: i32, decorated: bool) -> (i32, i32) {
    if decorated {
        (
            content_width + 2 * BORDER,
            TITLE_HEIGHT + content_height + BORDER,
        )
    } else {
        (content_width, content_height)
    }
}

/// 由外框尺寸计算装饰布局；`decorated = false` 时外框即内容本身。
pub fn layout(outer_width: i32, outer_height: i32, decorated: bool) -> Layout {
    if !decorated {
        return Layout {
            outer_width,
            outer_height,
            title_bar: Rect::new(0, 0, 0, 0),
            close_button: Rect::new(0, 0, 0, 0),
            maximize_button: Rect::new(0, 0, 0, 0),
            minimize_button: Rect::new(0, 0, 0, 0),
            content_origin: (0, 0),
        };
    }
    let step = BUTTON_SIZE + BUTTON_GAP;
    let close_x1 = outer_width - BORDER - BUTTON_SIZE;
    Layout {
        outer_width,
        outer_height,
        title_bar: Rect::new(0, 0, outer_width, TITLE_HEIGHT),
        close_button: Rect::new(close_x1, 0, close_x1 + BUTTON_SIZE, BUTTON_SIZE),
        maximize_button: Rect::new(
            close_x1 - step,
            0,
            close_x1 - step + BUTTON_SIZE,
            BUTTON_SIZE,
        ),
        minimize_button: Rect::new(
            close_x1 - 2 * step,
            0,
            close_x1 - 2 * step + BUTTON_SIZE,
            BUTTON_SIZE,
        ),
        content_origin: (BORDER, TITLE_HEIGHT),
    }
}

/// [`paint`] 的绘制参数（参数对象，避免长参数签名）。
pub struct Paint<'a> {
    /// 窗口外框的屏幕坐标原点。
    pub outer: (i32, i32),
    /// 装饰布局（相对外框原点）。
    pub layout: &'a Layout,
    /// 标题文字。
    pub title: &'a [u8],
    /// 是否为键盘焦点窗口（标题栏配色）。
    pub focused: bool,
    /// 是否最大化（最大化按钮图形切换为 ❐）。
    pub maximized: bool,
    /// 当前按住的按钮（画按下态）。
    pub pressed: Option<Button>,
}

/// 把装饰（标题栏 + 边框 + 三按钮 + 标题文字）画进 scanout，
/// 只写 `clip` 覆盖的像素。
pub fn paint(frame: &mut Frame, font: &UiFont, desc: &Paint<'_>, clip: Rect) {
    let outer = desc.outer;
    let window_layout = desc.layout;
    let (gradient_top, gradient_bottom) = if desc.focused {
        (TITLE_FOCUSED_TOP, TITLE_FOCUSED_BOTTOM)
    } else {
        (TITLE_UNFOCUSED_TOP, TITLE_UNFOCUSED_BOTTOM)
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
    paint_title_bar(frame, outer_rect, gradient_top, gradient_bottom, clip);
    // 左 / 右 / 下边框取标题栏渐变底色（跟随标题栏色系）。
    let left = Rect::new(
        outer.0,
        outer.1 + TITLE_HEIGHT,
        outer.0 + BORDER,
        outer_rect.y2,
    );
    let right = Rect::new(
        outer_rect.x2 - BORDER,
        outer.1 + TITLE_HEIGHT,
        outer_rect.x2,
        outer_rect.y2,
    );
    let bottom = Rect::new(
        outer.0,
        outer_rect.y2 - BORDER,
        outer_rect.x2,
        outer_rect.y2,
    );
    fill(frame, left.intersect(clip), gradient_bottom);
    fill(frame, right.intersect(clip), gradient_bottom);
    fill(frame, bottom.intersect(clip), gradient_bottom);
    for (button, rect) in [
        (Button::Close, window_layout.close_button),
        (Button::Maximize, window_layout.maximize_button),
        (Button::Minimize, window_layout.minimize_button),
    ] {
        let rect = shift(rect, outer);
        paint_button(
            frame,
            button,
            rect,
            desc.maximized,
            desc.pressed == Some(button),
            rect.intersect(clip),
        );
    }
    paint_title(frame, font, outer, window_layout, desc.title, clip);
}

/// 标题栏：垂直渐变 + 顶部两角圆角（圆角像素不画，露出下层）。
fn paint_title_bar(frame: &mut Frame, outer: Rect, top: u32, bottom: u32, clip: Rect) {
    let bar = Rect::new(outer.x1, outer.y1, outer.x2, outer.y1 + TITLE_HEIGHT).intersect(clip);
    if bar.is_empty() {
        return;
    }
    for y in bar.y1..bar.y2 {
        let color = gradient(top, bottom, y - outer.y1, TITLE_HEIGHT);
        let row = frame.row(y as usize);
        for x in bar.x1..bar.x2 {
            if in_rounded_top(x - outer.x1, y - outer.y1, outer.width()) {
                row[x as usize] = color;
            }
        }
    }
}

/// 标题栏局部坐标是否在顶部圆角保留区内（两上角半径 [`CORNER_RADIUS`] 的
/// 圆外像素不画）。
fn in_rounded_top(x: i32, y: i32, width: i32) -> bool {
    if y >= CORNER_RADIUS {
        return true;
    }
    let radius_sq = CORNER_RADIUS * CORNER_RADIUS;
    if x < CORNER_RADIUS {
        let (dx, dy) = (CORNER_RADIUS - x, CORNER_RADIUS - y);
        return dx * dx + dy * dy <= radius_sq;
    }
    if x >= width - CORNER_RADIUS {
        let (dx, dy) = (x + 1 + CORNER_RADIUS - width, CORNER_RADIUS - y);
        return dx * dx + dy * dy <= radius_sq;
    }
    true
}

/// 按钮：命中单元内居中画 48x48（1× 基准 24x24）圆角渐变方块（关闭红 / 其余蓝，
/// 按下态压暗为 3/4 亮度），再画白色图形（X / □|❐ / —），只写 `clip` 内像素。
fn paint_button(
    frame: &mut Frame,
    button: Button,
    cell: Rect,
    maximized: bool,
    pressed: bool,
    clip: Rect,
) {
    if clip.is_empty() {
        return;
    }
    let inset = (BUTTON_SIZE - BUTTON_VISUAL) / 2;
    let visual = Rect::new(
        cell.x1 + inset,
        cell.y1 + inset,
        cell.x1 + inset + BUTTON_VISUAL,
        cell.y1 + inset + BUTTON_VISUAL,
    );
    let (top, bottom) = if button == Button::Close {
        (CLOSE_TOP, CLOSE_BOTTOM)
    } else {
        (BUTTON_TOP, BUTTON_BOTTOM)
    };
    let area = visual.intersect(clip);
    for y in area.y1..area.y2 {
        let mut color = gradient(top, bottom, y - visual.y1, BUTTON_VISUAL);
        if pressed {
            color = darken(color);
        }
        let row = frame.row(y as usize);
        for x in area.x1..area.x2 {
            if !in_rounded_rect(x - visual.x1, y - visual.y1, BUTTON_VISUAL, BUTTON_VISUAL) {
                continue;
            }
            if glyph_on(button, maximized, x - visual.x1, y - visual.y1) {
                row[x as usize] = TEXT;
            } else {
                row[x as usize] = color;
            }
        }
    }
}

/// 按钮可视方块局部坐标是否在圆角保留区内（四角半径 [`BUTTON_RADIUS`]）。
fn in_rounded_rect(x: i32, y: i32, width: i32, height: i32) -> bool {
    let radius_sq = BUTTON_RADIUS * BUTTON_RADIUS;
    let cx = if x < BUTTON_RADIUS {
        BUTTON_RADIUS - x
    } else if x >= width - BUTTON_RADIUS {
        x + 1 + BUTTON_RADIUS - width
    } else {
        return true;
    };
    let cy = if y < BUTTON_RADIUS {
        BUTTON_RADIUS - y
    } else if y >= height - BUTTON_RADIUS {
        y + 1 + BUTTON_RADIUS - height
    } else {
        return true;
    };
    cx * cx + cy * cy <= radius_sq
}

/// 按钮白色图形命中：可视方块局部坐标（48x48，1× 基准 24x24）是否落在图形上。
/// 笔画几何常数均为 1× 基准值乘 [`SCALE`]，保证 2× 下形状不变。
fn glyph_on(button: Button, maximized: bool, x: i32, y: i32) -> bool {
    let lo = GLYPH_MARGIN;
    let hi = BUTTON_VISUAL - GLYPH_MARGIN;
    // 斜线半宽（1× 基准 1）。
    let line = SCALE;
    // 轮廓笔画宽度（1× 基准 2）。
    let outline = 2 * SCALE;
    // ❐ 前后框错位（1× 基准 3）。
    let stack = 3 * SCALE;
    match button {
        Button::Close => {
            let span = hi - lo - 1;
            let on_span = (lo..hi).contains(&x) && (lo..hi).contains(&y);
            on_span && ((x - y).abs() <= line || (x + y - 2 * lo - span).abs() <= line)
        }
        Button::Maximize if !maximized => {
            // □：outline 宽度轮廓方框。
            (lo..hi).contains(&x)
                && (lo..hi).contains(&y)
                && (x < lo + outline || x >= hi - outline || y < lo + outline || y >= hi - outline)
        }
        Button::Maximize => {
            // ❐：前框（右下）轮廓压住后框（左上）轮廓。
            let front_lo = lo + stack;
            let in_front = (front_lo..hi).contains(&x) && (front_lo..hi).contains(&y);
            let front = in_front
                && (x < front_lo + outline
                    || x >= hi - outline
                    || y < front_lo + outline
                    || y >= hi - outline);
            let back_hi = hi - stack;
            let in_back = (lo..back_hi).contains(&x) && (lo..back_hi).contains(&y);
            let back = in_back
                && !in_front
                && (x < lo + outline
                    || x >= back_hi - outline
                    || y < lo + outline
                    || y >= back_hi - outline);
            front || back
        }
        Button::Minimize => (lo..hi).contains(&x) && (hi - 4 * SCALE..hi - 2 * SCALE).contains(&y),
    }
}

/// 标题文字：uifont bold32 白字 alpha blend，右缘不超过最小化按钮，裁到 `clip`。
fn paint_title(
    frame: &mut Frame,
    font: &UiFont,
    outer: (i32, i32),
    window_layout: &Layout,
    title: &[u8],
    clip: Rect,
) {
    let Ok(text) = core::str::from_utf8(title) else {
        return;
    };
    let text_right = outer.0 + window_layout.minimize_button.x1;
    let area = Rect::new(
        outer.0 + TEXT_INDENT,
        outer.1,
        text_right,
        outer.1 + TITLE_HEIGHT,
    )
    .intersect(clip);
    if area.is_empty() {
        return;
    }
    // bold32 在 64px 标题栏内垂直居中。
    let face = Face::Bold32;
    let baseline =
        outer.1 + (TITLE_HEIGHT - font.ascent(face) - font.descent(face)) / 2 + font.ascent(face);
    font.draw(
        frame,
        face,
        TEXT,
        (outer.0 + TEXT_INDENT, baseline),
        text,
        area,
    );
}

/// 垂直渐变：`y` ∈ [0, height) 在 top→bottom 间线性插值。
fn gradient(top: u32, bottom: u32, y: i32, height: i32) -> u32 {
    let mix = |top: u32, bottom: u32| {
        (top * (height - 1 - y) as u32 + bottom * y as u32) / (height.max(1) - 1).max(1) as u32
    };
    let red = mix(top >> 16 & 0xff, bottom >> 16 & 0xff);
    let green = mix(top >> 8 & 0xff, bottom >> 8 & 0xff);
    let blue = mix(top & 0xff, bottom & 0xff);
    red << 16 | green << 8 | blue
}

/// 按下态压暗（各通道乘 3/4）。
fn darken(color: u32) -> u32 {
    let red = (color >> 16 & 0xff) * 3 / 4;
    let green = (color >> 8 & 0xff) * 3 / 4;
    let blue = (color & 0xff) * 3 / 4;
    red << 16 | green << 8 | blue
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
