//! 外壳的弹出层：托盘音量面板与右键菜单，共用一张全屏 Overlay 表面；坐标都是屏幕逻辑像素。
use crate::start::Place;
use canvas::Rect;
use classic_ui::{MenuItemState, Painter, Toplevel, ToplevelId, ToplevelRequest};

const MENU_WIDTH: i32 = 176;
const ITEM_HEIGHT: i32 = 20;
const SEPARATOR_HEIGHT: i32 = 8;
/// 托盘音量面板大小；位置由图标决定。
pub const VOLUME_PANEL: (i32, i32) = (64, 150);

/// 点击菜单项的结果。
#[derive(Clone, Copy)]
pub enum Action {
    MinimizeAll,
    TaskManager,
    /// 打开完整的"音量控制"窗口。
    VolumeProperties,
    Open(Place),
    /// 重新读取已安装的程序（开始菜单的内容）。
    Refresh,
    Window(ToplevelId, ToplevelRequest),
}

pub struct Item {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
    pub separator_before: bool,
}

fn item(label: &'static str, action: Action) -> Item {
    Item {
        label,
        shortcut: "",
        action,
        separator_before: false,
    }
}

pub struct ContextMenu {
    pub rect: Rect,
    pub items: Vec<Item>,
    pub hover: Option<usize>,
}

pub enum Overlay {
    /// 音量面板，值是面板矩形。
    Volume(Rect),
    Menu(ContextMenu),
}

/// 任务栏空白处。层叠/平铺窗口需要 compositor 提供窗口摆放协议，尚未实现，不显示灰色占位项。
pub fn taskbar_items() -> Vec<Item> {
    let mut tasks = item("任务管理器(K)", Action::TaskManager);
    tasks.separator_before = true;
    vec![item("最小化所有窗口(M)", Action::MinimizeAll), tasks]
}

/// 任务按钮：窗口的系统菜单。只列当前状态下有意义的项。
pub fn window_items(id: ToplevelId, info: &Toplevel) -> Vec<Item> {
    let mut items = Vec::new();
    if info.minimized {
        items.push(item(
            "还原(R)",
            Action::Window(id, ToplevelRequest::Minimize(false)),
        ));
    } else if info.maximized {
        items.push(item(
            "还原(R)",
            Action::Window(id, ToplevelRequest::Maximize(false)),
        ));
    }
    if !info.minimized {
        items.push(item(
            "最小化(N)",
            Action::Window(id, ToplevelRequest::Minimize(true)),
        ));
    }
    if !info.maximized {
        items.push(item(
            "最大化(X)",
            Action::Window(id, ToplevelRequest::Maximize(true)),
        ));
    }
    let mut close = item("关闭(C)", Action::Window(id, ToplevelRequest::Close));
    close.shortcut = "Alt+F4";
    close.separator_before = true;
    items.push(close);
    items
}

pub fn tray_volume_items() -> Vec<Item> {
    vec![item("调整音频属性(A)", Action::VolumeProperties)]
}

pub fn icon_items(place: Place) -> Vec<Item> {
    vec![item("打开(O)", Action::Open(place))]
}

pub fn desktop_items() -> Vec<Item> {
    vec![item("刷新(E)", Action::Refresh)]
}

fn row_height(item: &Item) -> i32 {
    ITEM_HEIGHT
        + if item.separator_before {
            SEPARATOR_HEIGHT
        } else {
            0
        }
}

impl ContextMenu {
    /// 把菜单放在 `origin`（指针位置）：右侧或下方放不下时翻转到指针另一侧，并保证完整落在屏幕内。
    pub fn new(origin: (i32, i32), items: Vec<Item>, screen: (i32, i32)) -> Self {
        let height = items.iter().map(row_height).sum::<i32>() + 4;
        let x = if origin.0 + MENU_WIDTH > screen.0 {
            origin.0 - MENU_WIDTH
        } else {
            origin.0
        };
        let y = if origin.1 + height > screen.1 {
            origin.1 - height
        } else {
            origin.1
        };
        Self {
            rect: Rect::new(x.max(0), y.max(0), MENU_WIDTH, height),
            items,
            hover: None,
        }
    }

    /// 各项的行矩形（含分隔线占位的项，矩形只覆盖项本身）。
    pub fn rows(&self) -> Vec<Rect> {
        let mut y = self.rect.y + 2;
        self.items
            .iter()
            .map(|item| {
                if item.separator_before {
                    y += SEPARATOR_HEIGHT;
                }
                let row = Rect::new(self.rect.x + 2, y, self.rect.width - 4, ITEM_HEIGHT);
                y += ITEM_HEIGHT;
                row
            })
            .collect()
    }

    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.rows().iter().position(|row| {
            x >= f64::from(row.x)
                && x < f64::from(row.right())
                && y >= f64::from(row.y)
                && y < f64::from(row.bottom())
        })
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        p.menu_popup(self.rect);
        for (index, (item, row)) in self.items.iter().zip(self.rows()).enumerate() {
            if item.separator_before {
                p.menu_separator(Rect::new(
                    self.rect.x + 2,
                    row.y - SEPARATOR_HEIGHT,
                    self.rect.width - 4,
                    SEPARATOR_HEIGHT,
                ));
            }
            p.menu_item(
                row,
                item.label,
                item.shortcut,
                MenuItemState {
                    highlighted: self.hover == Some(index),
                    ..Default::default()
                },
            );
        }
    }
}

/// 托盘音量图标正上方的面板位置：右缘不超出屏幕，底边在任务栏之上。
pub fn volume_panel(icon_center_x: i32, screen: (i32, i32), taskbar: i32) -> Rect {
    let (width, height) = VOLUME_PANEL;
    let x = (icon_center_x - width / 2).clamp(0, (screen.0 - width).max(0));
    Rect::new(x, screen.1 - taskbar - height, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menus_flip_instead_of_leaving_the_screen() {
        let screen = (800, 600);
        let normal = ContextMenu::new((100, 100), taskbar_items(), screen);
        assert_eq!((normal.rect.x, normal.rect.y), (100, 100));
        let corner = ContextMenu::new((790, 590), taskbar_items(), screen);
        assert_eq!(corner.rect.right(), 790);
        assert_eq!(corner.rect.bottom(), 590);
        let top_left = ContextMenu::new((0, 0), desktop_items(), screen);
        assert_eq!((top_left.rect.x, top_left.rect.y), (0, 0));
    }

    #[test]
    fn rows_follow_separators_and_hit_testing_ignores_them() {
        let menu = ContextMenu::new((10, 10), taskbar_items(), (800, 600));
        let rows = menu.rows();
        assert_eq!(rows[0].y, 12);
        assert_eq!(rows[1].y - rows[0].bottom(), SEPARATOR_HEIGHT);
        assert_eq!(menu.hit(20., f64::from(rows[1].y + 3)), Some(1));
        assert_eq!(menu.hit(20., f64::from(rows[0].bottom() + 2)), None);
        assert_eq!(menu.rect.bottom(), rows[1].bottom() + 2);
    }

    #[test]
    fn the_volume_panel_sits_above_the_taskbar_inside_the_screen() {
        let panel = volume_panel(1200, (1280, 800), 28);
        assert_eq!(panel.bottom(), 800 - 28);
        assert!(panel.right() <= 1280);
        assert_eq!(volume_panel(2000, (1280, 800), 28).right(), 1280);
        assert_eq!(volume_panel(-5, (1280, 800), 28).x, 0);
    }
}
