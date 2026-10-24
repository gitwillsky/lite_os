use canvas::Rect;
use classic_theme::{Edge, SystemColor};

use crate::painter::{Ink, Painter};

/// 菜单项的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuItemState {
    pub highlighted: bool,
    pub disabled: bool,
    pub checked: bool,
    pub has_submenu: bool,
}

const MENU_CHECK: [&str; 7] = [
    "......#", ".....##", "#...##.", "##.##..", ".###...", "..#....", ".......",
];
const SUBMENU_ARROW: [&str; 7] = ["#...", "##..", "###.", "####", "###.", "##..", "#..."];

impl Painter<'_> {
    /// 下拉菜单外框与背景；返回内容区。
    pub fn menu_popup(&mut self, rect: Rect) -> Rect {
        let inner = self.edge(rect, Edge::Raised);
        self.fill(inner, SystemColor::Menu);
        inner
    }

    /// 一行菜单项：整行高亮左右各留 2 像素；左侧 16 像素放勾选标记，快捷键右对齐。
    pub fn menu_item(&mut self, row: Rect, label: &str, shortcut: &str, state: MenuItemState) {
        let band = Rect::new(row.x + 2, row.y, row.width - 4, row.height);
        let (background, text) = if state.highlighted {
            (SystemColor::Hilight, SystemColor::HilightText)
        } else {
            (SystemColor::Menu, SystemColor::MenuText)
        };
        self.fill(band, background);
        let ink = if state.disabled && !state.highlighted {
            Ink::Disabled
        } else {
            Ink::Color(text)
        };
        let text_y = row.y + (row.height - self.line_height()) / 2;
        if state.checked {
            let color = if state.highlighted {
                SystemColor::HilightText
            } else {
                SystemColor::MenuText
            };
            self.pixels(row.x + 6, row.y + (row.height - 7) / 2, &MENU_CHECK, color);
        }
        self.text(row.x + 2 + 16, text_y, label, ink, false);
        if state.has_submenu {
            let color = if state.highlighted {
                SystemColor::HilightText
            } else {
                SystemColor::MenuText
            };
            self.pixels(
                row.x + row.width - 4 - 4,
                row.y + (row.height - 7) / 2,
                &SUBMENU_ARROW,
                color,
            );
        } else if !shortcut.is_empty() {
            let width = self.logical_ceil(self.text_width(shortcut, false));
            self.text(
                row.x + row.width - 2 - 8 - width,
                text_y,
                shortcut,
                ink,
                false,
            );
        }
    }

    /// 菜单分隔线：蚀刻水平线，上下各留 3 像素；返回占用的行高。
    pub fn menu_separator(&mut self, row: Rect) -> i32 {
        self.fill(row, SystemColor::Menu);
        self.edge(
            Rect::new(row.x + 2, row.y + 3, row.width - 4, 2),
            Edge::Etched,
        );
        8
    }

    /// 菜单栏顶层项：悬停单层凸起、按下单层凹陷。
    pub fn menu_bar_item(&mut self, rect: Rect, label: &str, hovered: bool, open: bool) {
        self.fill(rect, SystemColor::Menu);
        if open {
            self.edge(rect, Edge::SunkenOuter);
        } else if hovered {
            self.edge(rect, Edge::RaisedInner);
        }
        let shift = i32::from(open);
        self.text_centered(
            Rect::new(rect.x + shift, rect.y + shift, rect.width, rect.height),
            label,
            Ink::Color(SystemColor::MenuText),
            false,
        );
    }
}
