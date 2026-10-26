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
/// DrawFrameControl(DFCS_BUTTONRADIO) 的 12×12 圆圈：S 外圈左上暗影、H 外圈右下高光、
/// K 内圈左上黑、L 内圈右下亮面、`.` 内部底色；由 Windows 2000 Marlett 字形 j/l 及其中心对称得到。
const RADIO: [&str; 12] = [
    "    SSSS    ",
    "  SSKKKKSS  ",
    " SKK....KKH ",
    " SK......LH ",
    "SK........LH",
    "SK........LH",
    "SK........LH",
    "SK........LH",
    " SK......LH ",
    " SLL....LLH ",
    "  HHLLLLHH  ",
    "    HHHH    ",
];
/// Marlett 字形 i：选中圆点，与 [`RADIO`] 同一网格。
const RADIO_DOT: [&str; 8] = [
    "            ",
    "            ",
    "            ",
    "            ",
    "     ##     ",
    "    ####    ",
    "    ####    ",
    "     ##     ",
];
/// DrawFrameControl(DFCS_BUTTONCHECK) 的勾：7 列、每列 3 像素高的竖条，位于框内 (3, 3)。
pub(crate) const CHECK: [&str; 7] = [
    "......#", ".....##", "#...###", "##.###.", "#####..", ".###...", "..#....",
];

impl Painter<'_> {
    pub fn push_button(&mut self, rect: Rect, label: &str, state: ButtonState) {
        let mut body = rect;
        if state.default {
            self.outline(rect, SystemColor::WindowFrame);
            body = rect.inset(1);
        }
        let offset = if state.pressed {
            // 按下：外圈黑、内圈阴影，内容向右下偏移 1 像素。默认按钮的黑框就是这圈外框，
            // 不能再叠一圈黑色。
            let shadow = if state.default {
                body
            } else {
                self.outline(body, SystemColor::WindowFrame);
                body.inset(1)
            };
            self.outline(shadow, SystemColor::ButtonShadow);
            self.fill(shadow.inset(1), SystemColor::ButtonFace);
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
            self.pixels(x + 3, y + 3, &CHECK, ink);
        }
    }

    /// 13 × 13 单元内的单选框，与 Windows 2000 `DrawFrameControl(DFCS_BUTTONRADIO)` 一致。
    ///
    /// 原版用 Marlett 字形 j/k/l/m/n/i 以 13 像素、不抗锯齿分层绘制：外圈左上暗影、右下高光，
    /// 内圈左上黑、右下亮面，内部窗口底色（禁用为按钮面色），选中时中心 4×4 圆点。
    /// 圆占单元的第 1..12 列、第 0..11 行，与字形在 GDI 单元中的位置相同。无错误返回。
    pub fn radio(&mut self, x: i32, y: i32, selected: bool, disabled: bool) {
        use SystemColor as C;
        for (row, line) in RADIO.iter().enumerate() {
            for (column, mark) in line.bytes().enumerate() {
                let color = match mark {
                    b'S' => C::ButtonShadow,
                    b'H' => C::ButtonHilight,
                    b'K' => C::ButtonDkShadow,
                    b'L' => C::ButtonLight,
                    b'.' if disabled => C::ButtonFace,
                    b'.' => C::Window,
                    _ => continue,
                };
                self.fill(
                    Rect::new(x + 1 + column as i32, y + row as i32, 1, 1),
                    color,
                );
            }
        }
        if selected {
            let ink = if disabled {
                C::ButtonShadow
            } else {
                C::WindowText
            };
            self.pixels(x + 1, y, &RADIO_DOT, ink);
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
            let label = self.fit(label, rect.width - 18);
            let width = self.logical_ceil(self.text_width(&label, false));
            self.fill(
                Rect::new(rect.x + 8, rect.y, width + 2, self.line_height()),
                SystemColor::ButtonFace,
            );
            self.text(
                rect.x + 9,
                rect.y,
                &label,
                Ink::Color(SystemColor::ButtonText),
                false,
            );
        }
    }

    /// 状态栏：在 `rect` 内排一行凹陷面板，每个面板一段文字。
    ///
    /// 除最后一个外，面板宽度由文字量出（文字宽 + 左右留白），最后一个占满剩余；总宽放不下时按比例缩窄，
    /// 每段文字都截断在自己的面板内，所以语言、字体或数值位数变化都不会画出面板。
    pub fn status_bar(&mut self, rect: Rect, panes: &[&str]) {
        const PADDING: i32 = 16;
        const MINIMUM: i32 = 40;
        let wanted: Vec<i32> = panes
            .iter()
            .map(|text| (self.logical_ceil(self.text_width(text, false)) + PADDING).max(MINIMUM))
            .collect();
        let leading: i32 = wanted[..wanted.len().saturating_sub(1)].iter().sum();
        let room = (rect.width - MINIMUM).max(0);
        let mut x = rect.x;
        for (index, text) in panes.iter().enumerate() {
            let last = index + 1 == panes.len();
            let width = if last {
                rect.right() - x
            } else if leading > room {
                wanted[index] * room / leading
            } else {
                wanted[index]
            };
            let pane = Rect::new(x + 1, rect.y + 1, (width - 2).max(0), rect.height - 2);
            self.edge(pane, Edge::SunkenOuter);
            let y = pane.y + (pane.height - self.line_height()) / 2;
            self.text_fit(
                pane.x + 4,
                y,
                pane.width - 8,
                text,
                Ink::Color(SystemColor::ButtonText),
                false,
            );
            x += width;
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
    /// 画一个顶部标签表头（comctl32 非主题样式）。
    ///
    /// # Parameters
    ///
    /// - `tab`: 表头的实际绘制矩形，由调用者的标签页布局给出。选中项已向两侧各外扩 2、上抬 2，
    ///   并向下覆盖页面主体（`Edge::RaisedSoft`）上边框的 2 行；这里先填按钮面色，即擦除该段边框，
    ///   使选中页与页面连成一体。未选中项下沿落在页面上边框之上，页面的高光线保持可见。
    /// - `label_area`: 文字与焦点框区域；文字居中，超宽以"..."截断并裁剪，括号助记键按提示状态加下划线。
    /// - `ink`: 文字墨色；禁用页传 [`Ink::Disabled`]。
    /// - `focused`: 控件拥有键盘焦点且这是选中项时，在 `label_area` 画点状焦点框。
    ///
    /// 左、上为单像素高光，右为外黑内暗影，两个上角各缩进一像素形成圆角；外层白线与
    /// `Edge::RaisedSoft` 页面的外层白线同列。无错误返回。
    pub fn tab(&mut self, tab: Rect, label_area: Rect, label: &str, ink: Ink, focused: bool) {
        use SystemColor as C;
        self.fill(tab, C::ButtonFace);
        let (right, bottom) = (tab.right(), tab.bottom());
        let side = bottom - tab.y - 2;
        for (rect, color) in [
            (Rect::new(tab.x, tab.y + 2, 1, side), C::ButtonHilight),
            (Rect::new(tab.x + 1, tab.y + 1, 1, 1), C::ButtonHilight),
            (
                Rect::new(tab.x + 2, tab.y, tab.width - 4, 1),
                C::ButtonHilight,
            ),
            (Rect::new(right - 2, tab.y + 1, 1, 1), C::ButtonDkShadow),
            (Rect::new(right - 1, tab.y + 2, 1, side), C::ButtonDkShadow),
            (Rect::new(right - 2, tab.y + 2, 1, side), C::ButtonShadow),
        ] {
            self.fill(rect, color);
        }
        self.label_centered(label_area, label, ink, false);
        if focused {
            self.focus_rect(label_area);
        }
    }
}

impl Painter<'_> {
    /// 绘制列表表头单元格；标题与排序标记共用单元格裁剪，不能覆盖相邻列。
    /// sort 为 Some(true) 表示升序；尺寸非正时不绘制，无错误返回。
    pub fn list_header_cell(&mut self, rect: Rect, title: &str, sort: Option<bool>, pressed: bool) {
        self.clipped(rect, |p| {
            p.fill(rect, SystemColor::ButtonFace);
            // comctl32 表头：常态 EDGE_RAISED|BF_SOFT，按下为 BDR_RAISEDOUTER|BF_FLAT 的阴影色平框。
            if pressed {
                p.outline(rect, SystemColor::ButtonShadow);
            } else {
                p.edge(rect, Edge::RaisedSoft);
            }
            let offset = i32::from(pressed);
            let reserve = if sort.is_some() { 14 } else { 0 };
            p.text_fit(
                rect.x + 4 + offset,
                rect.y + (rect.height - p.line_height()) / 2 + offset,
                (rect.width - 8 - reserve).max(0),
                title,
                Ink::Color(SystemColor::ButtonText),
                false,
            );
            if let Some(ascending) = sort {
                let x = rect.right() - 11;
                let y = rect.y + (rect.height - 3) / 2;
                let glyph = if ascending {
                    ["..#..", ".###.", "#####"]
                } else {
                    ["#####", ".###.", "..#.."]
                };
                p.pixels(x, y, &glyph, SystemColor::ButtonText);
            }
        });
    }
}
