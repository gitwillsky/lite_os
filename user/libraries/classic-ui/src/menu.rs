use canvas::Rect;
use classic_theme::{Edge, SystemColor};

use crate::{
    FontRole, Fonts,
    painter::{Ink, Painter},
};
use classic_theme::Theme;

/// 菜单项的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuItemState {
    pub highlighted: bool,
    pub disabled: bool,
    pub checked: bool,
    pub has_submenu: bool,
    /// 默认项（MFS_DEFAULT）：标签用粗体。
    pub default: bool,
}

/// 下拉/上下文菜单的度量，全部由主题的 `MenuHeight` 与菜单字体推导；下拉菜单与外壳菜单共用，
/// 外壳可以在创建弹出表面之前算出尺寸。
pub struct MenuMetrics<'a> {
    fonts: &'a Fonts,
    scale: i32,
    menu_height: i32,
}

impl<'a> MenuMetrics<'a> {
    /// USER32 弹出菜单内容距外框三像素；下拉菜单与外壳右键菜单共用。
    pub const MARGIN: i32 = 3;
    pub fn new(theme: &Theme, fonts: &'a Fonts, scale: i32) -> Self {
        Self {
            fonts,
            scale: scale.max(1),
            menu_height: theme.window.menu_height,
        }
    }

    fn logical(&self, physical: i32) -> i32 {
        (physical + self.scale - 1) / self.scale
    }

    /// USER32 普通弹出菜单项：菜单字体行高 + 4；菜单栏的 MenuHeight 不决定弹出菜单项高。
    pub fn item_height(&self) -> i32 {
        let (ascent, descent) = self.fonts.role(FontRole::Menu).metrics();
        self.logical(ascent + descent) + 4
    }

    /// 分隔条高：`MenuHeight / 2`。
    pub fn separator_height(&self) -> i32 {
        self.menu_height / 2
    }

    /// 按内容算菜单宽度（逻辑像素）：左侧勾选列与边框 20，最宽的“标签 + 间隔 + 快捷键”，右侧留白与边框 13。
    ///
    /// # Parameters
    ///
    /// - `items`: 各项的 `(标签, 快捷键, 是否默认项)`；没有快捷键传空串，默认项按粗体度量。
    pub fn width<'b>(&self, items: impl IntoIterator<Item = (&'b str, &'b str, bool)>) -> i32 {
        const LEFT: i32 = 20;
        const RIGHT: i32 = 13;
        const SHORTCUT_GAP: i32 = 12;
        let font = self.fonts.role(FontRole::Menu);
        let widest = items
            .into_iter()
            .map(|(label, shortcut, bold)| {
                let label = self.logical(font.measure(label, bold));
                let shortcut = self.logical(font.measure(shortcut, false));
                label
                    + if shortcut > 0 {
                        shortcut + SHORTCUT_GAP
                    } else {
                        0
                    }
            })
            .max()
            .unwrap_or(0);
        LEFT + widest + RIGHT
    }
}

const SUBMENU_ARROW: [&str; 7] = ["#...", "##..", "###.", "####", "###.", "##..", "#..."];

impl Painter<'_> {
    /// 下拉菜单外框与背景；返回内容区。
    pub fn menu_popup(&mut self, rect: Rect) -> Rect {
        let inner = self.edge(rect, Edge::Raised);
        self.fill(inner, SystemColor::Menu);
        inner
    }

    /// 按内容算菜单宽度（逻辑像素），规则见 [`MenuMetrics::width`]。
    pub fn menu_width<'a>(&self, items: impl IntoIterator<Item = (&'a str, &'a str, bool)>) -> i32 {
        MenuMetrics::new(self.theme, self.fonts, self.scale).width(items)
    }

    /// 一行菜单项：高亮填满内容行，外框留白由弹出菜单几何统一提供；勾选列后为标签，快捷键右对齐。
    pub fn menu_item(&mut self, row: Rect, label: &str, shortcut: &str, state: MenuItemState) {
        self.with_role(FontRole::Menu, |p| {
            p.menu_item_in_font(row, label, shortcut, state)
        })
    }

    fn menu_item_in_font(&mut self, row: Rect, label: &str, shortcut: &str, state: MenuItemState) {
        let (background, text) = if state.highlighted {
            (SystemColor::Hilight, SystemColor::HilightText)
        } else {
            (SystemColor::Menu, SystemColor::MenuText)
        };
        self.fill(row, background);
        // 禁用项：常态为浮雕灰字；高亮时 Win2000 在选择色底上画 GrayText，而不是可用项的 HilightText。
        let ink = match (state.disabled, state.highlighted) {
            (true, false) => Ink::Disabled,
            (true, true) => Ink::Color(SystemColor::GrayText),
            _ => Ink::Color(text),
        };
        let text_y = row.y + (row.height - self.line_height()) / 2;
        if state.checked {
            let color = if state.highlighted {
                SystemColor::HilightText
            } else {
                SystemColor::MenuText
            };
            self.pixels(
                row.x + 6,
                row.y + (row.height - 7) / 2,
                &crate::controls::CHECK,
                color,
            );
        }
        // 标签不得压到右侧的快捷键或子菜单箭头：预留 17 像素标签偏移、快捷键宽度与间隔。
        let shortcut_width = self.logical_ceil(self.text_width(shortcut, false));
        let reserved = 17
            + 10
            + if shortcut_width > 0 {
                shortcut_width + 12
            } else {
                0
            };
        if state.default {
            self.label_fit_bold(row.x + 17, text_y, row.width - reserved, label, ink);
        } else {
            self.label_fit(row.x + 17, text_y, row.width - reserved, label, ink);
        }
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

    /// 菜单分隔线：在 `row` 内垂直居中的 2 像素蚀刻水平线；返回占用的行高。
    pub fn menu_separator(&mut self, row: Rect) -> i32 {
        self.fill(row, SystemColor::Menu);
        self.edge(
            Rect::new(row.x + 1, row.y + row.height / 2, row.width - 2, 2),
            Edge::Etched,
        );
        row.height
    }

    /// 菜单栏顶层项：悬停单层凸起、按下单层凹陷。
    pub fn menu_bar_item(&mut self, rect: Rect, label: &str, hovered: bool, open: bool) {
        self.with_role(FontRole::Menu, |p| {
            p.menu_bar_item_in_font(rect, label, hovered, open)
        })
    }

    fn menu_bar_item_in_font(&mut self, rect: Rect, label: &str, hovered: bool, open: bool) {
        self.fill(rect, SystemColor::Menu);
        if open {
            self.edge(rect, Edge::SunkenOuter);
        } else if hovered {
            self.edge(rect, Edge::RaisedInner);
        }
        let shift = i32::from(open);
        self.label_centered(
            Rect::new(rect.x + shift, rect.y + shift, rect.width, rect.height),
            label,
            Ink::Color(SystemColor::MenuText),
            false,
        );
    }
}
