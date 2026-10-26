use canvas::Rect;
use classic_theme::{Edge, SystemColor};

use crate::Bitmap;
use crate::{
    ButtonState,
    painter::{Ink, Painter},
};

/// 窗口边框种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// 可缩放：`EDGE_RAISED` 外加 2 像素面色，共 4 像素。
    Resizable,
    /// 对话框：`EDGE_RAISED` 外加 1 像素面色，共 3 像素。
    Fixed,
}

impl FrameKind {
    /// 外框占用的逻辑像素宽度：可缩放窗口 4，对话框 3。
    pub fn border(self) -> i32 {
        match self {
            Self::Resizable => 4,
            Self::Fixed => 3,
        }
    }
}

pub(crate) const CAPTION_HEIGHT: i32 = 19;

/// 标题栏按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

const MINIMIZE: [&str; 2] = ["######", "######"];
const MAXIMIZE: [&str; 9] = [
    "#########",
    "#########",
    "#.......#",
    "#.......#",
    "#.......#",
    "#.......#",
    "#.......#",
    "#.......#",
    "#########",
];
const RESTORE: [&str; 10] = [
    "..#######.",
    "..#######.",
    "..#.....#.",
    "#######.#.",
    "#######.#.",
    "#.....###.",
    "#.....#...",
    "#.....#...",
    "#######...",
    "..........",
];
const CLOSE: [&str; 7] = [
    "##....##", ".##..##.", "..####..", "...##...", "..####..", ".##..##.", "##....##",
];

/// 标题栏按钮的逻辑尺寸。
const BUTTON_WIDTH: i32 = 16;
const BUTTON_HEIGHT: i32 = 14;

pub(crate) fn caption_button_rects(
    rect: Rect,
    buttons: &[(CaptionButton, bool)],
) -> Vec<(CaptionButton, bool, Rect)> {
    let mut right_edge = rect.right() - 2;
    let mut areas = Vec::with_capacity(buttons.len());
    for (index, (button, enabled)) in buttons.iter().enumerate().rev() {
        if index + 1 < buttons.len()
            && matches!(button, CaptionButton::Maximize | CaptionButton::Restore)
        {
            right_edge -= 2;
        }
        let area = Rect::new(
            right_edge - BUTTON_WIDTH,
            rect.y + (rect.height - 1 - BUTTON_HEIGHT) / 2,
            BUTTON_WIDTH,
            BUTTON_HEIGHT,
        );
        areas.push((*button, *enabled, area));
        right_edge -= BUTTON_WIDTH;
    }
    areas
}

impl Painter<'_> {
    /// 画窗口外框并返回客户区外侧的矩形（外框以内）。
    pub fn window_frame(&mut self, rect: Rect, kind: FrameKind) -> Rect {
        let inner = self.edge(rect, Edge::Raised);
        let band = match kind {
            FrameKind::Resizable => 2,
            FrameKind::Fixed => 1,
        };
        let content = inner.inset(band);
        // 面色带 = 外框以内、客户区以外的环。
        self.fill(
            Rect::new(inner.x, inner.y, inner.width, band),
            SystemColor::ActiveBorder,
        );
        self.fill(
            Rect::new(inner.x, inner.bottom() - band, inner.width, band),
            SystemColor::ActiveBorder,
        );
        self.fill(
            Rect::new(inner.x, inner.y + band, band, inner.height - 2 * band),
            SystemColor::ActiveBorder,
        );
        self.fill(
            Rect::new(
                inner.right() - band,
                inner.y + band,
                band,
                inner.height - 2 * band,
            ),
            SystemColor::ActiveBorder,
        );
        content
    }

    /// 标题栏：渐变底、图标位、标题文字、右侧按钮、下方 1 像素分隔线；`rect` 高 19（含分隔线）。
    ///
    /// # Parameters
    ///
    /// - `icon`: 画在图标位（16×16）的程序图标；`None` 时留空。
    /// - `buttons`: 从左到右的按钮，通常是 `[最小化, 最大化|还原, 关闭]`，最后一个贴右边距 2 像素。
    pub fn caption_bar(
        &mut self,
        rect: Rect,
        title: &str,
        icon: Option<&Bitmap>,
        active: bool,
        buttons: &[(CaptionButton, bool)],
        pressed: Option<CaptionButton>,
    ) {
        let bar = Rect::new(rect.x, rect.y, rect.width, rect.height - 1);
        let (left, right, ink) = if active {
            (
                SystemColor::ActiveTitle,
                SystemColor::GradientActiveTitle,
                SystemColor::TitleText,
            )
        } else {
            (
                SystemColor::InactiveTitle,
                SystemColor::GradientInactiveTitle,
                SystemColor::InactiveTitleText,
            )
        };
        let (physical, from, to) = (self.physical(bar), self.color(left), self.color(right));
        self.surface.gradient_horizontal(physical, from, to);
        self.fill(
            Rect::new(rect.x, rect.bottom() - 1, rect.width, 1),
            SystemColor::ButtonFace,
        );

        let mut leftmost = bar.right() - 2;
        for (button, enabled, area) in caption_button_rects(rect, buttons) {
            self.caption_button(area, button, enabled, pressed == Some(button));
            leftmost = area.x;
        }

        if let Some(icon) = icon {
            icon.paint(self, rect.x + 2, bar.y + (bar.height - 16) / 2);
        }
        let text_x = rect.x + 2 + 16 + 2;
        let available = (leftmost - 2 - text_x).max(0) * self.scale;
        self.with_role(crate::FontRole::Caption, |p| {
            let shown = p.truncate_to_width(title, available, true);
            let text_y = bar.y + (bar.height - p.line_height()) / 2;
            p.text(text_x, text_y, &shown, Ink::Color(ink), true);
        });
    }

    fn caption_button(&mut self, area: Rect, button: CaptionButton, enabled: bool, pressed: bool) {
        self.push_button(
            area,
            "",
            ButtonState {
                pressed: pressed && enabled,
                disabled: !enabled,
                ..ButtonState::default()
            },
        );
        let (glyph, mut dx, mut dy): (&[&str], i32, i32) = match button {
            CaptionButton::Minimize => (&MINIMIZE, 4, 9),
            CaptionButton::Maximize => (&MAXIMIZE, 3, 2),
            CaptionButton::Restore => (&RESTORE, 3, 2),
            CaptionButton::Close => (&CLOSE, 4, 3),
        };
        if pressed && enabled {
            dx += 1;
            dy += 1;
        }
        if enabled {
            self.pixels(area.x + dx, area.y + dy, glyph, SystemColor::ButtonText);
        } else {
            self.pixels(
                area.x + dx + 1,
                area.y + dy + 1,
                glyph,
                SystemColor::ButtonHilight,
            );
            self.pixels(area.x + dx, area.y + dy, glyph, SystemColor::ButtonShadow);
        }
    }

    /// 超出 `max_width`（物理像素）时截断并追加"..."。
    pub(crate) fn truncate_to_width(&self, text: &str, max_width: i32, bold: bool) -> String {
        if self.text_width(text, bold) <= max_width {
            return text.to_owned();
        }
        let ellipsis = "...";
        // 连省略号也放不下时，只保留能完整容纳的点；零宽区域不发布文字。
        if self.text_width(ellipsis, bold) > max_width {
            return (1..=ellipsis.len())
                .take_while(|length| self.text_width(&ellipsis[..*length], bold) <= max_width)
                .last()
                .map_or_else(String::new, |length| ellipsis[..length].to_owned());
        }
        let budget = max_width - self.text_width(ellipsis, bold);
        let mut out = String::new();
        let mut used = 0;
        for character in text.chars() {
            let step = self.text_width(character.encode_utf8(&mut [0; 4]), bold);
            if used + step > budget {
                break;
            }
            used += step;
            out.push(character);
        }
        out + ellipsis
    }
}
