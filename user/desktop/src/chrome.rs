//! SSD 窗口装饰：布局纯函数 + 裁剪绘制（XP SP3 Luna 1:1 视觉）。
//!
//! 布局（1× 基准值，×[`SCALE`]）：外框 3px 边框（Luna 窗口 padding），标题栏
//! 总高 28px（3px 边框条 + 25px 渐变 header）；右上角从右到左依次为关闭、
//! 最大化 / 还原、最小化三个按钮，命中单元 23px（22px 可视方块 + 1px 间距），
//! 关闭按钮右缘距外框 3px 边框再内缩 1px。
//!
//! Luna 视觉（色值取自 winXP 复刻项目对 SP3 的实测）：header 为多段垂直渐变
//! （焦点 #0058EE→#003092 共 15 段，非焦点 #7697E7→#ABBAE3 共 13 段），外框
//! 边框纯色（焦点 #0831D9 / 非焦点 #6582F5），顶部两角圆角半径 8px；按钮为
//! 径向渐变（圆心在 90%,90%）+ 1px 白边 + 3px 圆角 + 底部内阴影（蓝钮
//! #4646FF / 红钮 #DA4600），悬停增亮 6/5、按下压暗 9/10；标题 bold24 白字
//! 带 1px 黑阴影。

use crate::{
    scanout::{Frame, Rect},
    uifont::{Face, UiFont},
};

/// 桌面统一 HiDPI 缩放因子：所有 UI 度量（chrome / taskbar / startmenu /
/// cursor / pointer 命中带与尺寸下限等）以 1× 基准值乘 `SCALE` 得到，本常量
/// 是缩放因子的唯一 owner。显示 mode 与指针坐标不缩放（屏幕绝对坐标）。
pub const SCALE: i32 = 2;

/// 标题栏高度（px，1× 基准 28 = 3px 边框条 + 25px header）。
pub const TITLE_HEIGHT: i32 = 28 * SCALE;
/// 左 / 右 / 下边框宽度（px，1× 基准 3，Luna 窗口 padding）。
pub const BORDER: i32 = 3 * SCALE;
/// 标题栏按钮命中单元边长（px，1× 基准 23 = 22px 按钮 + 1px 间距）。
pub const BUTTON_SIZE: i32 = 23 * SCALE;

/// 按钮可视方块边长（px，1× 基准 22，XP caption button）。
const BUTTON_VISUAL: i32 = 22 * SCALE;
/// 标题栏顶部圆角半径（px，1× 基准 8）。
const CORNER_RADIUS: i32 = 8 * SCALE;
/// 按钮可视方块圆角半径（px，1× 基准 3）。
const BUTTON_RADIUS: i32 = 3 * SCALE;
/// 按钮白边厚度（px，1× 基准 1）。
const BUTTON_BORDER: i32 = SCALE;
/// 按钮底部内阴影高度（px，1× 基准 2）。
const BUTTON_SHADOW: i32 = 2 * SCALE;

/// 焦点 / 非焦点 header 多段垂直渐变（permille 位置 + 颜色，升序）。
const TITLE_FOCUSED: &[(u32, u32)] = &[
    (0, 0x0000_58ee),
    (40, 0x0035_93ff),
    (60, 0x0028_8eff),
    (80, 0x0012_7dff),
    (100, 0x0003_6ffc),
    (140, 0x0002_62ee),
    (200, 0x0000_57e5),
    (240, 0x0000_54e3),
    (560, 0x0000_55eb),
    (660, 0x0000_5bf5),
    (760, 0x0002_6afe),
    (860, 0x0000_62ef),
    (920, 0x0000_52d6),
    (940, 0x0000_40ab),
    (1000, 0x0000_3092),
];
const TITLE_UNFOCUSED: &[(u32, u32)] = &[
    (0, 0x0076_97e7),
    (30, 0x007e_9ee3),
    (60, 0x0094_afe8),
    (80, 0x0097_b4e9),
    (140, 0x0082_a5e4),
    (170, 0x007c_9fe2),
    (250, 0x0079_96de),
    (560, 0x007b_99e1),
    (810, 0x0082_a9e9),
    (890, 0x0080_a5e7),
    (940, 0x007b_96e1),
    (970, 0x007a_93df),
    (1000, 0x00ab_bae3),
];
/// 边框纯色（Luna 窗口 padding 色）。
const BORDER_FOCUSED: u32 = 0x0008_31d9;
const BORDER_UNFOCUSED: u32 = 0x0065_82f5;
/// 按钮径向渐变（permille 半径 + 颜色）：蓝钮（最小化 / 最大化）与红钮（关闭）。
const BUTTON_BLUE: &[(u32, u32)] = &[
    (0, 0x0000_54e9),
    (550, 0x0022_63d5),
    (700, 0x0044_79e4),
    (900, 0x00a3_bbec),
    (1000, 0x00ff_ffff),
];
const BUTTON_RED: &[(u32, u32)] = &[
    (0, 0x00cc_4600),
    (550, 0x00dc_6527),
    (700, 0x00cd_7546),
    (900, 0x00ff_ccb2),
    (1000, 0x00ff_ffff),
];
/// 按钮底部内阴影色。
const SHADOW_BLUE: u32 = 0x0046_46ff;
const SHADOW_RED: u32 = 0x00da_4600;
/// 最大化还原态（❐）后框内部填充色。
const RESTORE_BACKFILL: u32 = 0x0013_6dff;
const TEXT: u32 = 0x00ff_ffff;
const TEXT_SHADOW: u32 = 0;
/// 标题文字左缘相对外框的缩进（px，1× 基准 4，另加 3px 边框）。
const TEXT_INDENT: i32 = 4 * SCALE;

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
    // 关闭按钮命中单元右缘：外框右缘内退 3px 边框 + 1px 按钮外边距。
    let close_x2 = outer_width - BORDER - SCALE;
    Layout {
        outer_width,
        outer_height,
        title_bar: Rect::new(0, 0, outer_width, TITLE_HEIGHT),
        close_button: Rect::new(close_x2 - BUTTON_SIZE, 0, close_x2, TITLE_HEIGHT),
        maximize_button: Rect::new(
            close_x2 - 2 * BUTTON_SIZE,
            0,
            close_x2 - BUTTON_SIZE,
            TITLE_HEIGHT,
        ),
        minimize_button: Rect::new(
            close_x2 - 3 * BUTTON_SIZE,
            0,
            close_x2 - 2 * BUTTON_SIZE,
            TITLE_HEIGHT,
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
    /// 当前悬停的按钮（画增亮态；按下态优先）。
    pub hover: Option<Button>,
}

/// 把装饰（标题栏 + 边框 + 三按钮 + 标题文字）画进 scanout，
/// 只写 `clip` 覆盖的像素。
pub fn paint(frame: &mut Frame, font: &UiFont, desc: &Paint<'_>, clip: Rect) {
    let outer = desc.outer;
    let window_layout = desc.layout;
    let (stops, border_color) = if desc.focused {
        (TITLE_FOCUSED, BORDER_FOCUSED)
    } else {
        (TITLE_UNFOCUSED, BORDER_UNFOCUSED)
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
    // 3px 边框：顶部条（含圆角）与左 / 右 / 下条均为 Luna padding 纯色。
    paint_border(frame, outer_rect, border_color, clip);
    paint_header(frame, outer_rect, stops, clip);
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
            desc.pressed != Some(button) && desc.hover == Some(button),
            rect.intersect(clip),
        );
    }
    paint_title(frame, font, outer, window_layout, desc.title, clip);
}

/// 外框 3px 边框：顶部条两端按 [`CORNER_RADIUS`] 圆角收缩，左 / 右 / 下条直填。
fn paint_border(frame: &mut Frame, outer: Rect, color: u32, clip: Rect) {
    let top = Rect::new(outer.x1, outer.y1, outer.x2, outer.y1 + BORDER).intersect(clip);
    for y in top.y1..top.y2 {
        let row = frame.row(y as usize);
        for x in top.x1..top.x2 {
            if in_rounded_top(x - outer.x1, y - outer.y1, outer.width()) {
                row[x as usize] = color;
            }
        }
    }
    let left = Rect::new(
        outer.x1,
        outer.y1 + BORDER,
        outer.x1 + BORDER,
        outer.y2,
    );
    let right = Rect::new(
        outer.x2 - BORDER,
        outer.y1 + BORDER,
        outer.x2,
        outer.y2,
    );
    let bottom = Rect::new(
        outer.x1 + BORDER,
        outer.y2 - BORDER,
        outer.x2 - BORDER,
        outer.y2,
    );
    fill(frame, left.intersect(clip), color);
    fill(frame, right.intersect(clip), color);
    fill(frame, bottom.intersect(clip), color);
}

/// header 渐变区：[BORDER, width-BORDER) × [BORDER, TITLE_HEIGHT)，上缘随外框
/// 圆角收缩；多段垂直渐变按段内线性插值。
fn paint_header(frame: &mut Frame, outer: Rect, stops: &[(u32, u32)], clip: Rect) {
    let header = Rect::new(
        outer.x1 + BORDER,
        outer.y1 + BORDER,
        outer.x2 - BORDER,
        outer.y1 + TITLE_HEIGHT,
    )
    .intersect(clip);
    if header.is_empty() {
        return;
    }
    let height = TITLE_HEIGHT - BORDER;
    for y in header.y1..header.y2 {
        let color = ramp(stops, (y - outer.y1 - BORDER) * 1000 / (height - 1).max(1));
        let row = frame.row(y as usize);
        for x in header.x1..header.x2 {
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

/// 按钮可视方块相对命中单元的偏移（水平居中，垂直落在 header 区居中）。
fn visual_rect(cell: Rect) -> Rect {
    let header_height = TITLE_HEIGHT - BORDER;
    let x1 = cell.x1 + (BUTTON_SIZE - BUTTON_VISUAL) / 2;
    let y1 = cell.y1 + BORDER + (header_height - BUTTON_VISUAL) / 2;
    Rect::new(x1, y1, x1 + BUTTON_VISUAL, y1 + BUTTON_VISUAL)
}

/// 按钮：22x22（1× 基准）径向渐变圆角方块 + 1px 白边 + 底部内阴影，再画白色
/// 图形（X / □|❐ / —）；悬停增亮 6/5、按下压暗 9/10，只写 `clip` 内像素。
fn paint_button(
    frame: &mut Frame,
    button: Button,
    cell: Rect,
    maximized: bool,
    pressed: bool,
    hover: bool,
    clip: Rect,
) {
    if clip.is_empty() {
        return;
    }
    let visual = visual_rect(cell);
    let (stops, shadow) = if button == Button::Close {
        (BUTTON_RED, SHADOW_RED)
    } else {
        (BUTTON_BLUE, SHADOW_BLUE)
    };
    // 径向渐变圆心（可视方块的 90%,90% 处）与最远角（0,0）的距离（半径基准）。
    let center = BUTTON_VISUAL * 9 / 10;
    let radius_max = isqrt(2 * center * center);
    let area = visual.intersect(clip);
    for y in area.y1..area.y2 {
        let local_y = y - visual.y1;
        let row = frame.row(y as usize);
        for x in area.x1..area.x2 {
            let local_x = x - visual.x1;
            if !in_rounded_rect(local_x, local_y, BUTTON_VISUAL, BUTTON_VISUAL) {
                continue;
            }
            if let Some(ink) = glyph_on(button, maximized, local_x, local_y) {
                row[x as usize] = ink;
                continue;
            }
            let mut color = if in_button_border(local_x, local_y) {
                TEXT
            } else {
                let dx = center - local_x;
                let dy = center - local_y;
                let t = isqrt(dx * dx + dy * dy) * 1000 / radius_max.max(1);
                let base = ramp(stops, t.min(1000));
                if local_y >= BUTTON_VISUAL - BUTTON_SHADOW {
                    blend(base, shadow, 128)
                } else {
                    base
                }
            };
            if pressed {
                color = brighten(color, 9, 10);
            } else if hover {
                color = brighten(color, 6, 5);
            }
            row[x as usize] = color;
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

/// 可视方块局部坐标是否落在 1px 白边带（圆角向内收缩 [`BUTTON_BORDER`] 的区域外）。
fn in_button_border(x: i32, y: i32) -> bool {
    !in_rounded_rect_inset(x, y, BUTTON_VISUAL, BUTTON_VISUAL, BUTTON_BORDER)
}

/// `in_rounded_rect` 的 inset 变体：四角圆心内移 `inset`、半径不变。
fn in_rounded_rect_inset(x: i32, y: i32, width: i32, height: i32, inset: i32) -> bool {
    let radius = BUTTON_RADIUS;
    let lo = inset;
    let hi_x = width - inset;
    let hi_y = height - inset;
    if x < lo || x >= hi_x || y < lo || y >= hi_y {
        return false;
    }
    let cx = if x < lo + radius {
        lo + radius - x
    } else if x >= hi_x - radius {
        x + 1 + radius - hi_x
    } else {
        return true;
    };
    let cy = if y < lo + radius {
        lo + radius - y
    } else if y >= hi_y - radius {
        y + 1 + radius - hi_y
    } else {
        return true;
    };
    cx * cx + cy * cy <= radius * radius
}

/// 按钮图形：可视方块局部坐标（22x22，1× 基准）落在白色图形上返回 `Some`
/// （❐ 后框内部为 [`RESTORE_BACKFILL`]）。笔画几何均为 1× 基准值乘 [`SCALE`]，
/// 取自 winXP 对 SP3 的实测 CSS。
fn glyph_on(button: Button, maximized: bool, x: i32, y: i32) -> Option<u32> {
    let s = SCALE;
    match button {
        // X：中心 (10,10)，两条宽 2、长 16 的 45° 斜条（|u|≤1 且 |v|≤8，旋转坐标系）。
        Button::Close => {
            let dx = x - 10 * s;
            let dy = y - 10 * s;
            // (dx+dy)/√2 与 (dx-dy)/√2：用 ×724/1024 近似 1/√2。
            let u = (dx + dy) * 724 / 1024;
            let v = (dx - dy) * 724 / 1024;
            (u.abs() <= s && v.abs() <= 8 * s || v.abs() <= s && u.abs() <= 8 * s).then_some(TEXT)
        }
        // □：(4,4) 起 12x12 轮廓，顶边 3px，其余边 1px。
        Button::Maximize if !maximized => {
            let in_box = (4 * s..16 * s).contains(&x) && (4 * s..16 * s).contains(&y);
            let edge = y < 7 * s || x < 5 * s || x >= 15 * s;
            (in_box && edge).then_some(TEXT)
        }
        // ❐：后框 (4,7) 8x8（内部填 #136DFF）+ 前框 (7,4) 8x8（顶边 2px，其余 1px）。
        Button::Maximize => {
            let front = (7 * s..15 * s).contains(&x) && (4 * s..12 * s).contains(&y);
            if front {
                let edge = y < 6 * s || x < 8 * s || x >= 14 * s;
                return edge.then_some(TEXT);
            }
            let back = (4 * s..12 * s).contains(&x) && (7 * s..15 * s).contains(&y);
            if !back {
                return None;
            }
            let edge = y < 9 * s || x < 5 * s || x >= 11 * s;
            Some(if edge { TEXT } else { RESTORE_BACKFILL })
        }
        // —：(4,13) 起 8x3 横条。
        Button::Minimize => {
            ((4 * s..12 * s).contains(&x) && (13 * s..16 * s).contains(&y)).then_some(TEXT)
        }
    }
}

/// 标题文字：uifont bold24 白字 + 1px 黑阴影（1× 基准），右缘不超过最小化
/// 按钮，裁到 `clip`。
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
    let text_x = outer.0 + BORDER + TEXT_INDENT;
    let text_right = outer.0 + window_layout.minimize_button.x1;
    let area = Rect::new(text_x, outer.1 + BORDER, text_right, outer.1 + TITLE_HEIGHT)
        .intersect(clip);
    if area.is_empty() {
        return;
    }
    // bold24 在 25px（1× 基准）header 内垂直居中。
    let face = Face::Bold24;
    let header_height = TITLE_HEIGHT - BORDER;
    let baseline = outer.1
        + BORDER
        + (header_height - font.ascent(face) - font.descent(face)) / 2
        + font.ascent(face);
    // 先画阴影（偏移 1px，1× 基准）再画白字；阴影只在白字未覆盖处可见。
    font.draw(
        frame,
        face,
        TEXT_SHADOW,
        (text_x + SCALE, baseline + SCALE),
        text,
        area,
    );
    font.draw(frame, face, TEXT, (text_x, baseline), text, area);
}

/// 分段渐变：`t`（permille，0..=1000）在升序 stops 间线性插值。
fn ramp(stops: &[(u32, u32)], t: i32) -> u32 {
    let t = t.clamp(0, 1000) as u32;
    let mut previous = stops[0];
    for &stop in stops {
        if t <= stop.0 {
            let span = stop.0 - previous.0;
            if span == 0 {
                return stop.1;
            }
            return mix(previous.1, stop.1, t - previous.0, span);
        }
        previous = stop;
    }
    stops[stops.len() - 1].1
}

/// 两色按 `num/den` 线性混合（num=0 取 a，num=den 取 b）。
fn mix(a: u32, b: u32, num: u32, den: u32) -> u32 {
    let channel = |shift: u32| {
        ((a >> shift & 0xff) * (den - num) + (b >> shift & 0xff) * num) / den.max(1)
    };
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// 两色等比混合（`alpha` 为 b 的覆盖率 0..=255）。
fn blend(a: u32, b: u32, alpha: u32) -> u32 {
    mix(a, b, alpha, 255)
}

/// 亮度按 `num/den` 缩放（通道 clamp 到 255）：悬停 6/5、按下 9/10。
fn brighten(color: u32, num: u32, den: u32) -> u32 {
    let channel = |shift: u32| ((color >> shift & 0xff) * num / den).min(255);
    channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// 非负整数平方根（舍去小数）。
fn isqrt(value: i32) -> i32 {
    let mut root = 0i32;
    let mut bit = 1i32 << 30;
    let mut rest = value;
    while bit > rest {
        bit >>= 2;
    }
    while bit != 0 {
        if rest >= root + bit {
            rest -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
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
