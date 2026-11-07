//! 外壳的弹出层：托盘音量面板与右键菜单，共用一张全屏 Overlay 表面；坐标都是屏幕逻辑像素。
use canvas::Rect;
use classic_ui::{
    Display, Event, EventKind, KeyboardInteractivity, Layer, LayerSettings, MenuItemState, Painter,
    Toplevel, ToplevelId, ToplevelRequest, WindowId,
};
use std::io;

use super::{Desktop, TASKBAR_HEIGHT, clipboard, paint};

/// 点击菜单项的结果。
#[derive(Clone, Copy)]
pub enum Action {
    /// 打开真实 USB 设备列表与安全移除入口。
    UsbDevices,
    MinimizeAll,
    TaskManager,
    /// 打开完整的"音量控制"窗口。
    VolumeProperties,
    NetworkStatus(u32),
    DisableNetwork(u32),
    /// 打开桌面上第 n 个图标。
    OpenIcon(usize),
    /// 对桌面上第 n 个图标执行资源管理器动词（删除、重命名、属性）。
    IconVerb(usize, &'static str),
    /// 打开“系统属性”。
    SystemProperties,
    /// 在资源管理器中打开外壳位置。
    OpenPlace(crate::start::Place),
    RenameIcon(usize),
    DeleteIcon(usize),
    FileSelection {
        index: usize,
        cut: bool,
    },
    PasteFiles,
    EmptyRecycleBin,
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
    pub disabled: bool,
}

fn item(label: &'static str, action: Action) -> Item {
    Item {
        label,
        shortcut: "",
        action,
        separator_before: false,
        default: false,
        disabled: false,
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
/// netshell.dll 15101/15102 的以太网通知菜单。`index` 是打开菜单时已选中的可用链路。
pub fn tray_network_items(index: Option<u32>, disabling: bool) -> Vec<Item> {
    let mut open = item(
        "打开网络和拨号连接(O)",
        Action::OpenPlace(crate::start::Place::NetworkConnections),
    );
    let Some(index) = index else {
        return vec![open];
    };
    open.separator_before = true;
    let mut disable = item("禁用(D)", Action::DisableNetwork(index));
    disable.disabled = disabling;
    let mut status = item("状态(S)", Action::NetworkStatus(index));
    status.default = true;
    vec![disable, status, open]
}
/// USB 设备通知图标的菜单。
pub fn tray_usb_items() -> Vec<Item> {
    vec![item("拔出或弹出硬件(O)", Action::UsbDevices)]
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
        IconKind::Path | IconKind::File => {
            if matches!(kind, IconKind::File) {
                add(
                    "打开方式(H)...",
                    Action::IconVerb(index, "open-with"),
                    false,
                );
            }
            add("剪切(T)", Action::FileSelection { index, cut: true }, true);
            add(
                "复制(C)",
                Action::FileSelection { index, cut: false },
                false,
            );
            add("删除(D)", Action::DeleteIcon(index), true);
            add("重命名(M)", Action::RenameIcon(index), false);
            add("属性(R)", Action::IconVerb(index, "properties"), true);
        }
        IconKind::MyComputer => add("属性(R)", Action::SystemProperties, true),
        IconKind::NetworkPlaces => add(
            "属性(R)",
            Action::OpenPlace(crate::start::Place::NetworkConnections),
            true,
        ),
        IconKind::RecycleBin { full } => {
            let mut empty = item("清空回收站(B)", Action::EmptyRecycleBin);
            empty.disabled = !full;
            empty.separator_before = true;
            items.push(empty);
        }
        IconKind::Other => {}
    }
    items
}

/// 桌面图标的种类，决定右键菜单里有哪些项。
#[derive(Clone, Copy)]
pub enum IconKind {
    File,
    /// 桌面文件夹里的真实文件或文件夹。
    Path,
    MyComputer,
    NetworkPlaces,
    RecycleBin {
        full: bool,
    },
    /// 其余系统图标（例如我的文档）。
    Other,
}

pub fn desktop_items(can_paste: bool) -> Vec<Item> {
    let mut paste = item("粘贴(P)", Action::PasteFiles);
    paste.separator_before = true;
    paste.disabled = !can_paste;
    let mut new_folder = item("新建文件夹(F)", Action::NewFolder);
    new_folder.separator_before = true;
    let mut properties = item("属性(R)", Action::DisplayProperties);
    properties.separator_before = true;
    vec![
        item("排列图标(I)", Action::ArrangeIcons),
        item("刷新(E)", Action::Refresh),
        paste,
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

    /// 上下键在可执行项间循环，跳过禁用项。没有高亮时从相应边缘开始。
    pub fn step(&mut self, down: bool) {
        let count = self.items.len();
        let mut index = self
            .hover
            .unwrap_or(if down { count.saturating_sub(1) } else { 0 });
        for _ in 0..count {
            index = if down {
                (index + 1) % count
            } else {
                (index + count - 1) % count
            };
            if !self.items[index].disabled {
                self.hover = Some(index);
                return;
            }
        }
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
                    disabled: item.disabled,
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

impl Desktop {
    /// 关闭当前弹出层并释放其输入抓取。
    pub(super) fn close_overlay(&mut self, display: &mut Display) -> io::Result<()> {
        if let Some((window, overlay)) = self.overlay.take() {
            if matches!(overlay, Overlay::Volume(_)) {
                self.volume.cancel();
            }
            display.destroy_window(window)?;
        }
        Ok(())
    }

    fn open_overlay(&mut self, display: &mut Display, overlay: Overlay) -> io::Result<()> {
        self.close_overlay(display)?;
        if self.menu.is_some() {
            self.toggle_menu(display)?;
        }
        let window = display.create_layer(
            "org.liteos.popup",
            LayerSettings {
                layer: Layer::Overlay,
                exclusive_zone: -1,
                keyboard: KeyboardInteractivity::Exclusive,
                ..Default::default()
            },
        )?;
        self.overlay = Some((window, overlay));
        Ok(())
    }

    /// 弹出层表面上的事件：菜单点击项执行、点击外部关闭；音量面板拖动滑块、点击静音。
    pub(super) fn overlay_event(&mut self, display: &mut Display, event: Event) -> io::Result<()> {
        let window = event.window;
        match event.kind {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (window, x, y);
                match &mut self.overlay {
                    Some((_, Overlay::Menu(menu))) => menu.hover = menu.hit(x, y),
                    Some((_, Overlay::Volume(panel))) => {
                        self.volume.motion(&sndvol::Layout::popup(*panel), x, y)
                    }
                    None => {}
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                let (_, x, y) = self.pointer;
                match &self.overlay {
                    Some((_, Overlay::Menu(menu))) if !pressed => {
                        match menu.hit(x, y).map(|index| &menu.items[index]) {
                            Some(item) if !item.disabled => {
                                self.activate_context_item(display, item.action)?;
                            }
                            None => self.close_overlay(display)?,
                            _ => {}
                        }
                    }
                    Some((_, Overlay::Volume(panel))) => {
                        let panel = *panel;
                        let inside = sndvol::Contains::contains(&panel, x, y);
                        if pressed && inside {
                            self.volume
                                .button(&sndvol::Layout::popup(panel), x, y, true);
                        } else if !pressed && !inside && !self.volume.dragging() {
                            self.close_overlay(display)?;
                        } else if !pressed {
                            self.volume.cancel();
                        }
                    }
                    _ => {}
                }
            }
            EventKind::Key(key) if key.pressed => {
                if key.code == 1 {
                    self.close_overlay(display)?;
                } else {
                    match &mut self.overlay {
                        Some((_, Overlay::Volume(_))) => self.volume.key(key.code),
                        Some((_, Overlay::Menu(menu))) => match key.code {
                            103 | 108 => menu.step(key.code == 108),
                            28 if !key.repeat => {
                                let action = menu
                                    .hover
                                    .map(|index| &menu.items[index])
                                    .filter(|item| !item.disabled)
                                    .map(|item| item.action);
                                if let Some(action) = action {
                                    self.activate_context_item(display, action)?;
                                }
                            }
                            _ => {}
                        },
                        None => {}
                    }
                }
            }
            EventKind::Close | EventKind::Focus(false) => self.close_overlay(display)?,
            _ => {}
        }
        Ok(())
    }

    fn activate_context_item(&mut self, display: &mut Display, action: Action) -> io::Result<()> {
        // 剪贴板操作需要菜单仍持有焦点。其余操作先关闭菜单，再启动目标。
        if matches!(action, Action::FileSelection { .. } | Action::PasteFiles) {
            self.perform(display, action)?;
            self.close_overlay(display)
        } else {
            self.close_overlay(display)?;
            self.perform(display, action)
        }
    }

    /// 右键使用指针命中项，键盘使用当前选中项和它的网格位置。
    pub(super) fn context_menu(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
        window: WindowId,
        keyboard: bool,
    ) -> io::Result<()> {
        let Some(c) = display.configuration(window) else {
            return Ok(());
        };
        let (_, pointer_x, pointer_y) = self.pointer;
        let (x, y, hit) = if keyboard {
            let (x, y) = self.selected.map_or((0, 0), |index| {
                let cell = self.icons.rect(index);
                (cell.x + cell.width / 2, cell.bottom())
            });
            (
                f64::from(x.clamp(0, c.width)),
                f64::from(y.clamp(0, c.height)),
                self.selected,
            )
        } else if window == self.background {
            let expanded = self
                .selected
                .filter(|_| display.keyboard_focus() == Some(window));
            let hit = art
                .icon_at(&self.icons, c.scale, (pointer_x, pointer_y), expanded)?
                .map(|(index, _)| index);
            (pointer_x, pointer_y, hit)
        } else {
            let hit = paint::areas(self, display, window, c).iter().position(|r| {
                pointer_x >= f64::from(r.x)
                    && pointer_x < f64::from(r.right())
                    && pointer_y >= f64::from(r.y)
                    && pointer_y < f64::from(r.bottom())
            });
            (pointer_x, pointer_y, hit)
        };
        let (screen, origin) = if window == self.taskbar {
            let screen = Self::screen(display, self.background);
            (screen, (x as i32, screen.1 - TASKBAR_HEIGHT + y as i32))
        } else {
            ((c.width, c.height), (x as i32, y as i32))
        };
        let items = if window == self.taskbar {
            match hit {
                Some(paint::VOLUME) => tray_volume_items(),
                Some(paint::USB) => tray_usb_items(),
                Some(paint::NETWORK) => tray_network_items(
                    self.network
                        .current(&self.settings)
                        .filter(|link| link.state != network_config::LinkState::Disabled)
                        .map(|link| link.index),
                    self.network.disabling(),
                ),
                Some(paint::CLOCK) => return Ok(()),
                Some(n) if n >= paint::TASKS_FROM => {
                    let ids = paint::task_ids(self, display, c.width);
                    match ids
                        .get(n - paint::TASKS_FROM)
                        .and_then(|id| display.toplevels().find(|(t, _)| t == id))
                    {
                        Some((id, info)) => window_items(id, info),
                        None => return Ok(()),
                    }
                }
                Some(_) => return Ok(()),
                None => taskbar_items(),
            }
        } else if window == self.background {
            match hit {
                Some(index) => {
                    self.selected = Some(index);
                    icon_items(index, self.icon_kind(index))
                }
                None => desktop_items(self.paste.is_none() && clipboard::can_paste(display)),
            }
        } else {
            return Ok(());
        };
        let size = art.menu_size(&items, c.scale)?;
        let menu = ContextMenu::new(origin, items, screen, size);
        self.open_overlay(display, Overlay::Menu(menu))
    }

    /// 在音量图标上方打开托盘调节面板。
    pub(super) fn open_volume_panel(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
        icon_center_x: i32,
    ) -> io::Result<()> {
        self.volume = sndvol::Volume::open();
        let screen = Self::screen(display, self.background);
        let scale = display.configuration(self.taskbar).map_or(1, |c| c.scale);
        let mut measure_failed = None;
        let size = sndvol::Layout::popup_size(|text| match art.text_width(text, scale) {
            Ok(width) => width,
            Err(error) => {
                measure_failed = Some(error);
                0
            }
        });
        if let Some(error) = measure_failed {
            return Err(error);
        }
        let panel = volume_panel(icon_center_x, screen, TASKBAR_HEIGHT, size);
        self.open_overlay(display, Overlay::Volume(panel))
    }
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
        let top_left = ContextMenu::new((0, 0), desktop_items(false), screen, SIZE);
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
