//! 外壳的弹出层：托盘音量面板与右键菜单，根接收全屏输入，局部子表面绘图；坐标都是屏幕逻辑像素。
//!
//! 右键菜单使用与应用相同的共享 `classic_ui::MenuBar` 状态机（级联子菜单、延时展开、方向键、助记键、
//! 松开时执行），条目结构与文字取自 Windows 2000 简中 shell32.dll 的菜单资源。
use canvas::Rect;
use classic_ui::{
    Display, DropItem, DropMenu, Event, EventKind, KeyboardInteractivity, Layer, LayerSettings,
    MenuBar, MenuEvent, Painter, Toplevel, ToplevelId, ToplevelRequest, WindowId,
};
use std::{io, time::Instant};

use super::{Desktop, TASKBAR_HEIGHT, clipboard, icons::Arrange, paint, start};

/// 点击菜单项的结果。
#[derive(Clone, Copy, Debug)]
pub enum Action {
    /// 级联子菜单的父条目：只展开子菜单，共享状态机不会把它作为命令交出。
    Submenu,
    /// 打开真实 USB 设备列表与安全移除入口。
    UsbDevices,
    MinimizeAll,
    UndoMinimizeAll,
    ArrangeWindows(classic_ui::ShellArrangement),
    UndoArrangement,
    TaskManager,
    /// 打开完整的"音量控制"窗口。
    VolumeProperties,
    NetworkStatus(u32),
    DisableNetwork(u32),
    /// 打开桌面上第 n 个图标。
    OpenIcon(usize),
    /// 以“资源管理器”方式（带文件夹树）打开桌面上第 n 个图标（shell 的 explore 动词）。
    ExploreIcon(usize),
    /// 对桌面上第 n 个图标执行资源管理器动词（删除、重命名、属性）。
    IconVerb(usize, &'static str),
    /// 打开“系统属性”。
    SystemProperties,
    /// 在资源管理器中打开外壳位置。
    OpenPlace(crate::start::Place),
    /// 映射或断开网络驱动器（与资源管理器“工具”菜单共用同一向导）。
    NetworkDrive {
        disconnect: bool,
    },
    RenameIcon(usize),
    DeleteSelection,
    FileSelection {
        cut: bool,
    },
    /// 粘贴剪贴板里的文件：`into` 为目标文件夹图标，None 粘贴到桌面。
    PasteFiles {
        into: Option<usize>,
    },
    EmptyRecycleBin,
    /// 在桌面文件夹里新建文件夹。
    NewFolder,
    /// 打开“任务栏属性”。
    TaskbarProperties,
    /// 打开“显示属性”。
    DisplayProperties,
    /// “排列图标”子菜单：按依据重新排列。
    Arrange(Arrange),
    /// 切换“自动排列”。
    AutoArrange,
    /// “对齐图标”：把图标吸附到网格。桌面图标只落在网格单元上，执行后布局不变。
    LineUpIcons,
    /// 刷新桌面：重新读取桌面文件夹与已安装的程序。
    Refresh,
    Window(ToplevelId, ToplevelRequest),
}

type Item = DropItem<Action, String>;

fn item(label: &str, action: Action) -> Item {
    DropItem::new(label.to_owned(), String::new(), action)
}

fn default(label: &str, action: Action) -> Item {
    Item {
        default: true,
        ..item(label, action)
    }
}

fn disabled(item: Item, disabled: bool) -> Item {
    Item { disabled, ..item }
}

fn submenu(label: &str, children: Vec<Item>) -> Item {
    Item {
        children,
        ..item(label, Action::Submenu)
    }
}

/// 右键菜单：共享状态机与它的唯一一组条目（标题为空的单个 DropMenu）。
pub struct ContextMenu {
    pub entries: [DropMenu<Action, String>; 1],
    pub state: MenuBar,
}

/// 上下文菜单没有菜单栏：标题区高度为 0，共享状态机只处理弹出层。
const NO_BAR: Rect = Rect {
    x: 0,
    y: 0,
    width: 0,
    height: 0,
};

impl ContextMenu {
    /// 在 `origin` 打开菜单；定位、翻转与子菜单边界都按 `screen`（覆盖层逻辑尺寸）。
    pub fn open(p: &Painter<'_>, items: Vec<Item>, origin: (i32, i32), screen: (i32, i32)) -> Self {
        let mut menu = Self {
            entries: [DropMenu {
                title: String::new(),
                items,
            }],
            state: MenuBar::new(),
        };
        menu.state.show_popup(
            p,
            &menu.entries,
            origin,
            Rect::new(0, 0, screen.0, screen.1),
        );
        menu
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        self.state.paint(p, NO_BAR, &self.entries, (-1, -1));
    }

    /// 等待展开的子菜单的截止时刻（SPI_GETMENUSHOWDELAY），交给事件循环的超时。
    pub fn deadline(&self) -> Option<Instant> {
        self.state.deadline()
    }

    /// 剪贴板可用性变化时同步全部“粘贴”条目；返回是否需要重画。
    pub fn set_paste_enabled(&mut self, enabled: bool) -> bool {
        fn visit(items: &mut [Item], enabled: bool) -> bool {
            let mut changed = false;
            for item in items {
                if matches!(item.command, Action::PasteFiles { .. }) && item.disabled == enabled {
                    item.disabled = !enabled;
                    changed = true;
                }
                changed |= visit(&mut item.children, enabled);
            }
            changed
        }
        visit(&mut self.entries[0].items, enabled)
    }
}

/// 音量面板或右键菜单。
pub enum Overlay {
    /// 音量面板，值是面板矩形。
    Volume(Rect),
    Menu(ContextMenu),
}

/// 任务栏空白处：命令和撤销种类来自合成器的已授权外壳状态。
pub fn taskbar_items(display: &Display) -> Vec<Item> {
    use classic_ui::ShellArrangement;
    let mut items = vec![
        item(
            "层叠窗口(S)",
            Action::ArrangeWindows(ShellArrangement::Cascade),
        ),
        item(
            "横向平铺窗口(H)",
            Action::ArrangeWindows(ShellArrangement::Horizontal),
        ),
        item(
            "纵向平铺窗口(E)",
            Action::ArrangeWindows(ShellArrangement::Vertical),
        ),
        item("最小化所有窗口(M)", Action::MinimizeAll),
    ];
    if display.can_undo_minimize_all() {
        items.push(item("撤销最小化所有窗口(U)", Action::UndoMinimizeAll));
    } else if let Some(layout) = display.undo_arrangement_kind() {
        let label = if layout == ShellArrangement::Cascade {
            "撤销层叠(U)"
        } else {
            "撤销平铺(U)"
        };
        items.push(item(label, Action::UndoArrangement));
    }
    items.push(item("任务管理器(K)", Action::TaskManager).separated());
    items.push(item("属性(R)", Action::TaskbarProperties).separated());
    items
}

/// 任务按钮：窗口的系统菜单。只列当前状态下有意义的项。
pub fn window_items(id: ToplevelId, info: &Toplevel) -> Vec<Item> {
    let mut items = Vec::new();
    if info.minimized {
        items.push(item(
            "还原(R)",
            Action::Window(id, ToplevelRequest::Activate),
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
    items.push(
        DropItem::new(
            "关闭(C)".to_owned(),
            "Alt+F4".to_owned(),
            Action::Window(id, ToplevelRequest::Close),
        )
        .separated(),
    );
    items
}

pub fn tray_volume_items() -> Vec<Item> {
    vec![item("调整音频属性(A)", Action::VolumeProperties)]
}
/// netshell.dll 15101/15102 的以太网通知菜单。`index` 是打开菜单时已选中的可用链路。
pub fn tray_network_items(index: Option<u32>, disabling: bool) -> Vec<Item> {
    let open = item(
        "打开网络和拨号连接(O)",
        Action::OpenPlace(crate::start::Place::NetworkConnections),
    );
    let Some(index) = index else {
        return vec![open];
    };
    vec![
        disabled(item("禁用(D)", Action::DisableNetwork(index)), disabling),
        default("状态(S)", Action::NetworkStatus(index)),
        open.separated(),
    ]
}
/// USB 设备通知图标的菜单。
pub fn tray_usb_items() -> Vec<Item> {
    vec![item("拔出或弹出硬件(O)", Action::UsbDevices)]
}

/// 桌面图标的种类，决定右键菜单里有哪些项。
#[derive(Clone, Copy)]
pub enum IconKind {
    File,
    /// 桌面文件夹里的真实文件夹。
    Path,
    MyComputer,
    NetworkPlaces,
    RecycleBin {
        full: bool,
    },
    /// 其余系统图标（我的文档）。
    Other,
}

/// 桌面图标的右键菜单，按 shell32.dll 菜单 213（对象菜单）与各外壳文件夹的动词组织：
/// 默认动词粗体在最前；文件/文件夹有剪切、复制（文件夹另有粘贴）、删除、重命名与属性；
/// “我的电脑”“网上邻居”带网络驱动器命令（菜单 225）；回收站带“清空回收站”（菜单 227）。
pub fn icon_items(index: usize, kind: IconKind, can_paste: bool) -> Vec<Item> {
    let open = default("打开(O)", Action::OpenIcon(index));
    let explore = item("资源管理器(X)", Action::ExploreIcon(index));
    let properties = |action| item("属性(R)", action).separated();
    let network = || {
        [
            item(
                "映射网络驱动器(N)...",
                Action::NetworkDrive { disconnect: false },
            )
            .separated(),
            item(
                "断开网络驱动器(I)...",
                Action::NetworkDrive { disconnect: true },
            ),
        ]
    };
    match kind {
        IconKind::File => vec![
            open,
            item("打开方式(H)...", Action::IconVerb(index, "open-with")),
            item("剪切(T)", Action::FileSelection { cut: true }).separated(),
            item("复制(C)", Action::FileSelection { cut: false }),
            item("删除(D)", Action::DeleteSelection).separated(),
            item("重命名(M)", Action::RenameIcon(index)),
            properties(Action::IconVerb(index, "properties")),
        ],
        IconKind::Path => vec![
            open,
            explore,
            item("剪切(T)", Action::FileSelection { cut: true }).separated(),
            item("复制(C)", Action::FileSelection { cut: false }),
            disabled(
                item("粘贴(P)", Action::PasteFiles { into: Some(index) }),
                !can_paste,
            ),
            item("删除(D)", Action::DeleteSelection).separated(),
            item("重命名(M)", Action::RenameIcon(index)),
            properties(Action::IconVerb(index, "properties")),
        ],
        IconKind::MyComputer => {
            let mut items = vec![open, explore];
            items.extend(network());
            items.push(properties(Action::SystemProperties));
            items
        }
        IconKind::NetworkPlaces => {
            let mut items = vec![open, explore];
            items.extend(network());
            items.push(properties(Action::OpenPlace(
                crate::start::Place::NetworkConnections,
            )));
            items
        }
        IconKind::RecycleBin { full } => vec![
            open,
            explore,
            disabled(item("清空回收站(B)", Action::EmptyRecycleBin), !full).separated(),
        ],
        IconKind::Other => vec![open, explore],
    }
}

/// 桌面空白处的右键菜单，按 shell32.dll 菜单 215（背景菜单）与 221（排列依据）：
/// 排列图标 ▸、对齐图标、刷新 | 粘贴 | 新建 ▸ | 属性。自动排列打开时“对齐图标”不可用（与原版相同）。
pub fn desktop_items(can_paste: bool, arrange: Arrange, auto: bool) -> Vec<Item> {
    let by = |label: &str, value| item(label, Action::Arrange(value)).checked(arrange == value);
    vec![
        submenu(
            "排列图标(I)",
            vec![
                by("按名称(N)", Arrange::Name),
                by("按类型(T)", Arrange::Type),
                by("按大小(Z)", Arrange::Size),
                by("按日期(D)", Arrange::Date),
                item("自动排列(A)", Action::AutoArrange)
                    .separated()
                    .checked(auto),
            ],
        ),
        disabled(item("对齐图标(L)", Action::LineUpIcons), auto),
        item("刷新(E)", Action::Refresh),
        disabled(
            item("粘贴(P)", Action::PasteFiles { into: None }),
            !can_paste,
        )
        .separated(),
        submenu("新建(W)", vec![item("文件夹(F)", Action::NewFolder)]).separated(),
        item("属性(R)", Action::DisplayProperties).separated(),
    ]
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
    pub(super) fn toggle_menu(&mut self, display: &mut Display) -> io::Result<()> {
        if self.menu.visible().is_some() {
            self.retire_menu(display)?;
            // 在关闭时发出下一次 configure；直到打开才创建像素和提交缓冲。
            self.menu = start::Surface::Prepared(start::prepare_layer(display)?);
        } else {
            self.menu_selection = start::Selection::default();
            let window = match self.menu.window() {
                Some(window) => window,
                None => start::prepare_layer(display)?,
            };
            self.menu = start::Surface::Open(window);
        }
        Ok(())
    }

    pub(super) fn retire_menu(&mut self, display: &mut Display) -> io::Result<()> {
        let previous = std::mem::replace(&mut self.menu, start::Surface::Absent);
        if let Some(menu) = previous.window() {
            self.popup_surfaces.remove(&menu);
            display.destroy_window(menu)?;
        }
        if previous.visible().is_some() {
            // 栈的活动窗口由标准 foreign-toplevel 快照提供；关闭菜单后交还键盘，不另存焦点历史。
            let active = display
                .toplevels()
                .find(|(_, window)| window.active)
                .map(|(id, _)| id);
            if let Some(active) = active {
                display.control_toplevel(active, ToplevelRequest::Activate)?;
            }
        }
        Ok(())
    }
    /// 关闭当前弹出层并释放其输入抓取。
    pub(super) fn close_overlay(&mut self, display: &mut Display) -> io::Result<()> {
        if let Some((window, overlay)) = self.overlay.take() {
            if matches!(overlay, Overlay::Volume(_)) {
                self.volume.cancel();
            }
            self.popup_surfaces.remove(&window);
            display.destroy_window(window)?;
        }
        Ok(())
    }

    fn open_overlay(&mut self, display: &mut Display, overlay: Overlay) -> io::Result<()> {
        self.close_overlay(display)?;
        if self.menu.visible().is_some() {
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

    /// 弹出层表面上的事件：菜单交给共享状态机（左右键松开执行、按下菜单外关闭）；
    /// 音量面板拖动滑块、点击静音。
    pub(super) fn overlay_event(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
        event: Event,
    ) -> io::Result<()> {
        let window = event.window;
        let scale = display.configuration(window).map_or(1, |c| c.scale);
        if let EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } = event.kind {
            self.pointer = (window, x, y);
        }
        let (_, x, y) = self.pointer;
        let point = (x.floor() as i32, y.floor() as i32);
        if let Some((_, Overlay::Menu(menu))) = &mut self.overlay {
            let result = art.with_probe(scale, |p| match &event.kind {
                EventKind::PointerEnter { .. } | EventKind::PointerMove { .. } => {
                    menu.state.motion(p, NO_BAR, &menu.entries, point);
                    MenuEvent::Handled
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => menu.state.press(p, NO_BAR, &menu.entries, point),
                // TrackPopupMenu 的 TPM_RIGHTBUTTON：外壳上下文菜单左右键都可选择，松开时执行。
                EventKind::Button {
                    code: 0x110 | 0x111,
                    pressed: false,
                    ..
                } => menu.state.release(p, &menu.entries, point),
                EventKind::Key(key) if key.pressed => menu.state.key(p, NO_BAR, &menu.entries, key),
                EventKind::Key(key) => {
                    menu.state.key_released(key);
                    MenuEvent::Handled
                }
                _ => MenuEvent::Ignored,
            })?;
            let active = menu.state.active();
            return match result {
                MenuEvent::Command(action) => self.activate_context_item(display, action),
                _ if !active
                    || matches!(event.kind, EventKind::Close | EventKind::Focus(false)) =>
                {
                    self.close_overlay(display)
                }
                _ => Ok(()),
            };
        }
        match event.kind {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                if let Some((_, Overlay::Volume(panel))) = &self.overlay {
                    self.volume.motion(&sndvol::Layout::popup(*panel), x, y);
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                if let Some((_, Overlay::Volume(panel))) = &self.overlay {
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
            }
            EventKind::Key(key) if key.pressed => {
                if key.code == 1 {
                    self.close_overlay(display)?;
                } else if let Some((_, Overlay::Volume(_))) = &self.overlay {
                    self.volume.key(key.code);
                }
            }
            EventKind::Close | EventKind::Focus(false) => self.close_overlay(display)?,
            _ => {}
        }
        Ok(())
    }

    fn activate_context_item(&mut self, display: &mut Display, action: Action) -> io::Result<()> {
        // 剪贴板操作需要菜单仍持有焦点。其余操作先关闭菜单，再启动目标。
        if matches!(
            action,
            Action::FileSelection { .. } | Action::PasteFiles { .. }
        ) {
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
        let (x, y, hit) = if keyboard && window == self.taskbar {
            let ids = paint::task_ids(self, display, c.width);
            let Some(index) = ids.iter().position(|id| Some(*id) == self.task_focus) else {
                return Ok(());
            };
            let index = paint::TASKS_FROM + index;
            let rect = paint::areas(self, display, window, c)[index];
            (
                f64::from(rect.x + rect.width / 2),
                f64::from(rect.y),
                Some(index),
            )
        } else if keyboard {
            let (x, y) = self.focused.map_or((0, 0), |index| {
                let cell = self.icons.rect(index);
                (cell.x + cell.width / 2, cell.bottom())
            });
            (
                f64::from(x.clamp(0, c.width)),
                f64::from(y.clamp(0, c.height)),
                self.focused,
            )
        } else if window == self.background {
            let expanded = self.focused.filter(|index| {
                self.selection.contains(index) && display.keyboard_focus() == Some(window)
            });
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
        let can_paste = self.paste.is_none() && clipboard::can_paste(display);
        let items = if window == self.taskbar {
            match hit {
                Some(paint::VOLUME) => tray_volume_items(),
                Some(paint::USB) => tray_usb_items(),
                Some(paint::NETWORK) => tray_network_items(
                    self.tray
                        .network
                        .current(&self.settings)
                        .filter(|link| link.state != network_config::LinkState::Disabled)
                        .map(|link| link.index),
                    self.tray.network.disabling(),
                ),
                Some(paint::CLOCK) => return Ok(()),
                Some(n) if n >= paint::TASKS_FROM => {
                    let ids = paint::task_ids(self, display, c.width);
                    let Some(id) = ids.get(n - paint::TASKS_FROM) else {
                        return Ok(());
                    };
                    let Some(info) = display.toplevel(*id) else {
                        return Ok(());
                    };
                    window_items(*id, info)
                }
                Some(_) => return Ok(()),
                None => taskbar_items(display),
            }
        } else if window == self.background {
            match hit {
                Some(index) => {
                    if self.selection.contains(&index) {
                        self.focused = Some(index);
                    } else {
                        self.select_icon(index, false, false, false);
                    }
                    icon_items(index, self.icon_kind(index), can_paste)
                }
                None => {
                    self.selection.clear();
                    self.focused = None;
                    self.selection_anchor = None;
                    desktop_items(can_paste, self.icons.arrange, self.icons.auto)
                }
            }
        } else {
            return Ok(());
        };
        let menu = art.with_probe(c.scale, |p| ContextMenu::open(p, items, origin, screen))?;
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
