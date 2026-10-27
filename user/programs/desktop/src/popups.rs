//! 外壳的弹出层：托盘音量面板与右键菜单，共用一张全屏 Overlay 表面；坐标都是屏幕逻辑像素。
use canvas::Rect;
use classic_ui::{MenuItemState, Painter, Toplevel, ToplevelId, ToplevelRequest};

/// 点击菜单项的结果。
#[derive(Clone, Copy)]
pub enum Action {
    MinimizeAll,
    TaskManager,
    /// 打开完整的"音量控制"窗口。
    VolumeProperties,
    /// 打开桌面上第 n 个图标。
    OpenIcon(usize),
    /// 对桌面上第 n 个图标执行资源管理器动词（删除、重命名、属性）。
    IconVerb(usize, &'static str),
    /// 打开“系统属性”。
    SystemProperties,
    /// 在资源管理器中打开外壳位置。
    OpenPlace(crate::start::Place),
    /// 在桌面文件夹里新建文件夹。
    NewFolder,
    /// 打开“任务栏属性”。
    TaskbarProperties,
    /// 打开“显示属性”。
    DisplayProperties,
    /// 清除保存的图标位置并重新排列。
    ArrangeIcons,
    /// 重新读取已安装的程序（开始菜单的内容）。
    Refresh,
    Window(ToplevelId, ToplevelRequest),
}

pub struct Item {
    pub label: &'static str,
    pub shortcut: &'static str,
    pub action: Action,
    pub separator_before: bool,
    /// 默认项（双击图标等价的命令），粗体显示。
    pub default: bool,
}

fn item(label: &'static str, action: Action) -> Item {
    Item {
        label,
        shortcut: "",
        action,
        separator_before: false,
        default: false,
    }
}

pub struct ContextMenu {
    pub rect: Rect,
    pub items: Vec<Item>,
    pub hover: Option<usize>,
    size: MenuSize,
}

/// 菜单尺寸：由共享 `classic_ui::MenuMetrics` 按主题与菜单字体算出，外壳不另设常量。
#[derive(Clone, Copy)]
pub struct MenuSize {
    pub width: i32,
    pub item_height: i32,
    pub separator_height: i32,
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
    let mut properties = item("属性(R)", Action::TaskbarProperties);
    properties.separator_before = true;
    vec![
        item("最小化所有窗口(M)", Action::MinimizeAll),
        tasks,
        properties,
    ]
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

/// 桌面图标的右键菜单：文件与文件夹有删除、重命名、属性（交给资源管理器）；“我的电脑”的属性是系统属性，
/// “网上邻居”的属性是“网络和拨号连接”文件夹（与 Windows 2000 相同）。
pub fn icon_items(index: usize, kind: IconKind) -> Vec<Item> {
    let mut open = item("打开(O)", Action::OpenIcon(index));
    open.default = true;
    let mut items = vec![open];
    let mut add = |label, action: Action, separated: bool| {
        let mut entry = item(label, action);
        entry.separator_before = separated;
        items.push(entry);
    };
    match kind {
        IconKind::Path => {
            add("删除(D)", Action::IconVerb(index, "delete"), true);
            add("重命名(M)", Action::IconVerb(index, "rename"), false);
            add("属性(R)", Action::IconVerb(index, "properties"), true);
        }
        IconKind::MyComputer => add("属性(R)", Action::SystemProperties, true),
        IconKind::NetworkPlaces => add(
            "属性(R)",
            Action::OpenPlace(crate::start::Place::NetworkConnections),
            true,
        ),
        IconKind::Other => {}
    }
    items
}

/// 桌面图标的种类，决定右键菜单里有哪些项。
#[derive(Clone, Copy)]
pub enum IconKind {
    /// 桌面文件夹里的真实文件或文件夹。
    Path,
    MyComputer,
    NetworkPlaces,
    /// 其余系统图标（我的文档、回收站……）。
    Other,
}

pub fn desktop_items() -> Vec<Item> {
    let mut new_folder = item("新建文件夹(F)", Action::NewFolder);
    new_folder.separator_before = true;
    let mut properties = item("属性(R)", Action::DisplayProperties);
    properties.separator_before = true;
    vec![
        item("排列图标(I)", Action::ArrangeIcons),
        item("刷新(E)", Action::Refresh),
        new_folder,
        properties,
    ]
}

impl ContextMenu {
    /// 把菜单放在 `origin`（指针位置）：右侧或下方放不下时翻转到指针另一侧，并保证完整落在屏幕内。
    pub fn new(origin: (i32, i32), items: Vec<Item>, screen: (i32, i32), size: MenuSize) -> Self {
        let height = items
            .iter()
            .map(|item| {
                size.item_height
                    + if item.separator_before {
                        size.separator_height
                    } else {
                        0
                    }
            })
            .sum::<i32>()
            + 2 * classic_ui::MenuMetrics::MARGIN;
        let x = if origin.0 + size.width > screen.0 {
            origin.0 - size.width
        } else {
            origin.0
        };
        let y = if origin.1 + height > screen.1 {
            origin.1 - height
        } else {
            origin.1
        };
        Self {
            rect: Rect::new(x.max(0), y.max(0), size.width, height),
            items,
            hover: None,
            size,
        }
    }

    /// 各项的行矩形（含分隔线占位的项，矩形只覆盖项本身）。
    pub fn rows(&self) -> Vec<Rect> {
        let mut y = self.rect.y + classic_ui::MenuMetrics::MARGIN;
        self.items
            .iter()
            .map(|item| {
                if item.separator_before {
                    y += self.size.separator_height;
                }
                let row = Rect::new(
                    self.rect.x + classic_ui::MenuMetrics::MARGIN,
                    y,
                    self.rect.width - 2 * classic_ui::MenuMetrics::MARGIN,
                    self.size.item_height,
                );
                y += self.size.item_height;
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
                    self.rect.x + classic_ui::MenuMetrics::MARGIN,
                    row.y - self.size.separator_height,
                    self.rect.width - 2 * classic_ui::MenuMetrics::MARGIN,
                    self.size.separator_height,
                ));
            }
            p.menu_item(
                row,
                item.label,
                item.shortcut,
                MenuItemState {
                    highlighted: self.hover == Some(index),
                    default: item.default,
                    ..Default::default()
                },
            );
        }
    }
}

/// 托盘音量图标正上方的面板位置：右缘不超出屏幕，底边在任务栏之上；`size` 是面板按内容量出的尺寸。
pub fn volume_panel(
    icon_center_x: i32,
    screen: (i32, i32),
    taskbar: i32,
    size: (i32, i32),
) -> Rect {
    let (width, height) = size;
    let x = (icon_center_x - width / 2).clamp(0, (screen.0 - width).max(0));
    Rect::new(x, screen.1 - taskbar - height, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: MenuSize = MenuSize {
        width: 176,
        item_height: 18,
        separator_height: 9,
    };

    #[test]
    fn menus_flip_instead_of_leaving_the_screen() {
        let screen = (800, 600);
        let normal = ContextMenu::new((100, 100), taskbar_items(), screen, SIZE);
        assert_eq!((normal.rect.x, normal.rect.y), (100, 100));
        let corner = ContextMenu::new((790, 590), taskbar_items(), screen, SIZE);
        assert_eq!(corner.rect.right(), 790);
        assert_eq!(corner.rect.bottom(), 590);
        let top_left = ContextMenu::new((0, 0), desktop_items(), screen, SIZE);
        assert_eq!((top_left.rect.x, top_left.rect.y), (0, 0));
    }

    #[test]
    fn rows_follow_separators_and_hit_testing_ignores_them() {
        let menu = ContextMenu::new((10, 10), taskbar_items(), (800, 600), SIZE);
        let rows = menu.rows();
        assert_eq!(rows[0].y, 13);
        assert_eq!(rows[1].y - rows[0].bottom(), SIZE.separator_height);
        assert_eq!(menu.hit(20., f64::from(rows[1].y + 3)), Some(1));
        assert_eq!(menu.hit(20., f64::from(rows[0].bottom() + 2)), None);
        assert_eq!(menu.rect.bottom(), rows.last().unwrap().bottom() + 3);
    }

    #[test]
    fn the_volume_panel_sits_above_the_taskbar_inside_the_screen() {
        const SIZE: (i32, i32) = (80, 150);
        let panel = volume_panel(1200, (1280, 800), 28, SIZE);
        assert_eq!(panel.bottom(), 800 - 28);
        assert!(panel.right() <= 1280);
        assert_eq!(volume_panel(2000, (1280, 800), 28, SIZE).right(), 1280);
        assert_eq!(volume_panel(-5, (1280, 800), 28, SIZE).x, 0);
    }
}
