use canvas::Rect;
use classic_theme::{Edge, SystemColor};

use crate::painter::{Ink, Painter};

/// 按钮的瞬时状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ButtonState {
    pub pressed: bool,
    /// 默认按钮：外加一圈黑框。
    pub default: bool,
    pub focused: bool,
    pub disabled: bool,
}

/// 滚动条的可点击部件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    ArrowStart,
    ArrowEnd,
    TrackBefore,
    Thumb,
    TrackAfter,
}

/// 滚动条沿轴方向的尺寸（逻辑像素）：箭头按钮为正方形，边长等于滚动条宽。
pub const SCROLLBAR_WIDTH: i32 = 16;
const MIN_THUMB: i32 = 8;

/// 滑块在轨道内的位置，逻辑像素，相对轨道起点。
///
/// 范围 `[0, maximum]`；`page` 是可见页大小，`position` 是当前位置，均以内容单位计。
/// `page >= maximum + 1`（内容不超出视口）时没有滑块，返回 `None`。
pub fn thumb_span(track: i32, maximum: i32, page: i32, position: i32) -> Option<(i32, i32)> {
    let total = i64::from(maximum) + 1;
    if track <= 0 || maximum <= 0 || i64::from(page) >= total {
        return None;
    }
    let length = (i64::from(track) * i64::from(page) / total)
        .max(i64::from(MIN_THUMB))
        .min(i64::from(track));
    // 位置范围是 [0, maximum - page + 1]：最大位置时滑块贴齐轨道末端。
    let travel = total - i64::from(page);
    let offset = (i64::from(track) - length) * i64::from(position.clamp(0, travel as i32)) / travel;
    Some((offset as i32, length as i32))
}

const UP: [&str; 4] = ["...#...", "..###..", ".#####.", "#######"];
const DOWN: [&str; 4] = ["#######", ".#####.", "..###..", "...#..."];
const LEFT: [&str; 7] = ["...#", "..##", ".###", "####", ".###", "..##", "...#"];
const RIGHT: [&str; 7] = ["#...", "##..", "###.", "####", "###.", "##..", "#..."];
const CHECK: [&str; 6] = [
    "......#", ".....##", "#...##.", "##.##..", ".###...", "..#....",
];

impl Painter<'_> {
    pub fn push_button(&mut self, rect: Rect, label: &str, state: ButtonState) {
        let mut body = rect;
        if state.default {
            self.outline(rect, SystemColor::WindowFrame);
            body = rect.inset(1);
        }
        let offset = if state.pressed {
            // 按下：外圈黑、内圈阴影，内容向右下偏移 1 像素。
            self.outline(body, SystemColor::WindowFrame);
            self.outline(body.inset(1), SystemColor::ButtonShadow);
            self.fill(body.inset(2), SystemColor::ButtonFace);
            1
        } else {
            let face = self.edge(body, Edge::Raised);
            self.fill(face, SystemColor::ButtonFace);
            0
        };
        let ink = if state.disabled {
            Ink::Disabled
        } else {
            Ink::Color(SystemColor::ButtonText)
        };
        let content = Rect::new(body.x + offset, body.y + offset, body.width, body.height);
        self.label_centered(content, label, ink, false);
        if state.focused {
            self.focus_rect(body.inset(3));
        }
    }

    /// 13 × 13 的复选框；`checked` 为 `None` 表示未选中。
    pub fn checkbox(&mut self, x: i32, y: i32, checked: bool, disabled: bool) {
        let rect = Rect::new(x, y, 13, 13);
        let inner = self.edge(rect, Edge::Sunken);
        self.fill(
            inner,
            if disabled {
                SystemColor::ButtonFace
            } else {
                SystemColor::Window
            },
        );
        if checked {
            let ink = if disabled {
                SystemColor::GrayText
            } else {
                SystemColor::WindowText
            };
            self.pixels(x + 3, y + 4, &CHECK, ink);
        }
    }

    /// 13 × 13 的单选框：圆形的凹陷边框。
    pub fn radio(&mut self, x: i32, y: i32, selected: bool, disabled: bool) {
        for row in 0..13 {
            for column in 0..13 {
                let (dx, dy) = (column - 6, row - 6);
                let distance = dx * dx + dy * dy;
                let upper_left = dx + dy < 0;
                let color = match distance {
                    ..=20 if distance <= 5 && selected => {
                        if disabled {
                            SystemColor::GrayText
                        } else {
                            SystemColor::WindowText
                        }
                    }
                    ..=20 if disabled => SystemColor::ButtonFace,
                    ..=20 => SystemColor::Window,
                    21..=30 if upper_left => SystemColor::ButtonDkShadow,
                    21..=30 => SystemColor::ButtonLight,
                    31..=40 if upper_left => SystemColor::ButtonShadow,
                    31..=40 => SystemColor::ButtonHilight,
                    _ => continue,
                };
                self.fill(Rect::new(x + column, y + row, 1, 1), color);
            }
        }
    }

    /// 单行/多行编辑框的外观：凹陷边框加窗口底色；返回可用的内部区域。
    pub fn edit_frame(&mut self, rect: Rect, enabled: bool) -> Rect {
        let inner = self.edge(rect, Edge::Sunken);
        self.fill(
            inner,
            if enabled {
                SystemColor::Window
            } else {
                SystemColor::ButtonFace
            },
        );
        inner
    }

    /// 分组框：蚀刻边框，标题嵌在上边框中。
    pub fn group_box(&mut self, rect: Rect, label: &str) {
        let half = self.line_height() / 2;
        self.edge(
            Rect::new(rect.x, rect.y + half, rect.width, rect.height - half),
            Edge::Etched,
        );
        if !label.is_empty() {
            let width = self.logical_ceil(self.text_width(label, false));
            self.fill(
                Rect::new(rect.x + 8, rect.y, width + 2, self.line_height()),
                SystemColor::ButtonFace,
            );
            self.text(
                rect.x + 9,
                rect.y,
                label,
                Ink::Color(SystemColor::ButtonText),
                false,
            );
        }
    }

    /// 工具提示：`INFOBK` 底、黑色 1 像素框、四周留 2 像素。
    pub fn tooltip(&mut self, x: i32, y: i32, text: &str) -> Rect {
        let width = self.logical_ceil(self.text_width(text, false)) + 2 * 3;
        let rect = Rect::new(x, y, width, self.line_height() + 2 * 3);
        self.fill(rect, SystemColor::InfoWindow);
        self.outline(rect, SystemColor::WindowFrame);
        self.text(x + 3, y + 3, text, Ink::Color(SystemColor::InfoText), false);
        rect
    }

    /// 画滚动条，`rect` 沿轴方向为长边。`pressed` 是正被按住的部件。
    ///
    /// # Parameters
    ///
    /// - `maximum`、`page`、`position`: 见 [`thumb_span`]；`maximum <= 0` 视为禁用，只画轨道和灰箭头。
    pub fn scrollbar(
        &mut self,
        rect: Rect,
        vertical: bool,
        (maximum, page, position): (i32, i32, i32),
        pressed: Option<ScrollbarPart>,
    ) {
        let length = if vertical { rect.height } else { rect.width };
        let at = |offset: i32, size: i32| {
            if vertical {
                Rect::new(rect.x, rect.y + offset, rect.width, size)
            } else {
                Rect::new(rect.x + offset, rect.y, size, rect.height)
            }
        };
        let arrow = SCROLLBAR_WIDTH.min(length / 2);
        let track = (length - 2 * arrow).max(0);
        let thumb = thumb_span(track, maximum, page, position);
        let enabled = maximum > 0;
        let (low, high) = (
            self.color(SystemColor::Scrollbar),
            self.color(SystemColor::ButtonHilight),
        );
        let track_rect = self.physical(at(arrow, track));
        self.surface.checker(track_rect, self.scale, low, high);
        if let (Some(part), Some((offset, size))) = (pressed, thumb) {
            let range = match part {
                ScrollbarPart::TrackBefore => Some(at(arrow, offset)),
                ScrollbarPart::TrackAfter => Some(at(arrow + offset + size, track - offset - size)),
                _ => None,
            };
            if let Some(range) = range {
                let physical = self.physical(range);
                self.surface.invert(physical);
            }
        }
        if let Some((offset, size)) = thumb {
            let face = self.edge(at(arrow + offset, size), Edge::Raised);
            self.fill(face, SystemColor::ButtonFace);
        }
        let (start_glyph, end_glyph): (&[&str], &[&str]) = if vertical {
            (&UP, &DOWN)
        } else {
            (&LEFT, &RIGHT)
        };
        for (part, offset, glyph) in [
            (ScrollbarPart::ArrowStart, 0, start_glyph),
            (ScrollbarPart::ArrowEnd, length - arrow, end_glyph),
        ] {
            let button = at(offset, arrow);
            let is_pressed = pressed == Some(part) && enabled;
            if is_pressed {
                self.outline(button, SystemColor::ButtonShadow);
                self.fill(button.inset(1), SystemColor::ButtonFace);
            } else {
                let face = self.edge(button, Edge::Raised);
                self.fill(face, SystemColor::ButtonFace);
            }
            let glyph_width = glyph[0].len() as i32;
            let glyph_x = button.x + (button.width - glyph_width) / 2 + i32::from(is_pressed);
            let glyph_y =
                button.y + (button.height - glyph.len() as i32) / 2 + i32::from(is_pressed);
            if enabled {
                self.pixels(glyph_x, glyph_y, glyph, SystemColor::ButtonText);
            } else {
                self.pixels(glyph_x + 1, glyph_y + 1, glyph, SystemColor::ButtonHilight);
                self.pixels(glyph_x, glyph_y, glyph, SystemColor::ButtonShadow);
            }
        }
    }
}

impl Painter<'_> {
    /// 画一个标签头。
    ///
    /// `selected` 的标签头比页面高出 2 像素，下沿 `tab.bottom()` 应取页面上边框下方 2 像素以盖住它，因此没有底边、与页面连成一体；
    /// 未选中的标签头下沿取页面上边框的上沿，保留页面的高光线。第一个选中的标签头左边必须与页面左边框同一列，
    /// 其余选中的标签头向两侧各多出 2 像素，由调用者给出矩形。
    ///
    /// 左边是外亮面内高光（与页面边框同序），上边是外高光内亮面，右是黑与暗影，三个角各缩进一像素形成圆角；文字居中于顶部 22 像素（未选中为整个矩形）。
    pub fn tab(&mut self, tab: Rect, label: &str, selected: bool) {
        use SystemColor as C;
        self.fill(tab, C::ButtonFace);
        let (right, bottom) = (tab.right(), tab.bottom());
        let side = bottom - tab.y - 2;
        for (rect, color) in [
            // 左边两列的顺序与页面凸起边框一致（外灰内白），第一个标签的白线才与页面左边的白线同一列。
            (Rect::new(tab.x, tab.y + 2, 1, side), C::ButtonLight),
            (Rect::new(tab.x + 1, tab.y + 2, 1, side), C::ButtonHilight),
            (
                Rect::new(tab.x + 2, tab.y, tab.width - 4, 1),
                C::ButtonHilight,
            ),
            (
                Rect::new(tab.x + 2, tab.y + 1, tab.width - 4, 1),
                C::ButtonLight,
            ),
            (Rect::new(tab.x + 1, tab.y + 1, 1, 1), C::ButtonHilight),
            (Rect::new(right - 1, tab.y + 2, 1, side), C::ButtonDkShadow),
            (Rect::new(right - 2, tab.y + 2, 1, side), C::ButtonShadow),
            (Rect::new(right - 2, tab.y + 1, 1, 1), C::ButtonDkShadow),
        ] {
            self.fill(rect, color);
        }
        let text_height = if selected { 22 } else { tab.height };
        self.text_centered(
            Rect::new(tab.x, tab.y, tab.width, text_height),
            label,
            Ink::Color(C::ButtonText),
            false,
        );
    }
}
