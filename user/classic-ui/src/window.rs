use canvas::Rect;
use classic_theme::{Edge, SystemColor};

use crate::painter::{Ink, Painter};

/// 窗口边框种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// 可缩放：`EDGE_RAISED` 外加 2 像素面色，共 4 像素。
    Resizable,
    /// 对话框：`EDGE_RAISED` 外加 1 像素面色，共 3 像素。
    Fixed,
}

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
    /// - `buttons`: 从左到右的按钮，通常是 `[最小化, 最大化|还原, 关闭]`，最后一个贴右边距 2 像素。
    pub fn caption_bar(
        &mut self,
        rect: Rect,
        title: &str,
        active: bool,
        buttons: &[(CaptionButton, bool)],
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

        let mut right_edge = bar.right() - 2;
        let mut leftmost = right_edge;
        for (index, (button, enabled)) in buttons.iter().enumerate().rev() {
            // 最小化与最大化紧贴；关闭按钮与它们之间隔 2 像素。
            if index + 1 < buttons.len()
                && matches!(button, CaptionButton::Maximize | CaptionButton::Restore)
            {
                right_edge -= 2;
            }
            let area = Rect::new(
                right_edge - BUTTON_WIDTH,
                bar.y + (bar.height - BUTTON_HEIGHT) / 2,
                BUTTON_WIDTH,
                BUTTON_HEIGHT,
            );
            self.caption_button(area, *button, *enabled);
            right_edge -= BUTTON_WIDTH;
            leftmost = area.x;
        }

        let text_x = rect.x + 2 + 16 + 2;
        let available = (leftmost - 2 - text_x).max(0) * self.scale;
        let shown = self.truncate_to_width(title, available, true);
        let text_y = bar.y + (bar.height - self.line_height()) / 2;
        self.text(text_x, text_y, &shown, Ink::Color(ink), true);
    }

    fn caption_button(&mut self, area: Rect, button: CaptionButton, enabled: bool) {
        let face = self.edge(area, Edge::Raised);
        self.fill(face, SystemColor::ButtonFace);
        let (glyph, dx, dy): (&[&str], i32, i32) = match button {
            CaptionButton::Minimize => (&MINIMIZE, 4, 9),
            CaptionButton::Maximize => (&MAXIMIZE, 3, 2),
            CaptionButton::Restore => (&RESTORE, 3, 2),
            CaptionButton::Close => (&CLOSE, 4, 3),
        };
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
