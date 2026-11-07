//! 用户身份运行的桌面外壳；仅持有 logon 委派的 Shell 连接。
extern crate liteos_runtime as _;
mod clipboard;
mod dialogs;
mod icons;
mod network;
mod paint;
mod popups;
mod start;
mod taskbar;
mod usb;
use classic_ui::{
    Display, Edit, Event, EventKind, ITEM_DOUBLE_CLICK, ItemClick, KeyboardInteractivity, Layer,
    LayerSettings, ToplevelRequest, WindowId,
};
use desktop_entry::file_uris;
use dialogs::{Dialog, Kind};
use linux_uapi::shared_memory::MemFd;
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, ExitCode, Stdio},
    time::{Duration, Instant},
};

/// 任务栏高度（逻辑像素）；开始菜单贴在它上方。
const TASKBAR_HEIGHT: i32 = 28;

/// 开始菜单“程序”使用用户与系统安装条目；设置程序只在控制面板出现。
fn installed_programs() -> Vec<desktop_entry::Entry> {
    let mut apps = desktop_entry::installed();
    apps.retain(|app| !app.in_category("Settings"));
    apps
}

/// 一次图标拖动：按下时指针在图标内的偏移、当前指针位置，以及是否已移动足够远（区分点击与拖动）。
struct Drag {
    index: usize,
    grab: (i32, i32),
    pos: (i32, i32),
    moved: bool,
    /// 最近一次移动命中回收站；绘制高亮和释放按钮使用同一判定。
    recycle: bool,
    /// 按下时该项已选中，释放时点击标签可以请求重命名。
    was_selected: bool,
}

/// 编辑绑定到原始路径，目录刷新或重新排序不会改到另一项。
struct Rename {
    operation: desktop_entry::fileops::Rename,
    edit: Edit,
    selecting: bool,
}

struct Desktop {
    background: WindowId,
    taskbar: WindowId,
    menu: Option<WindowId>,
    menu_selection: start::Selection,
    /// 当前弹出层（托盘音量面板或右键菜单）及其全屏表面；同一时刻最多一个。
    overlay: Option<(WindowId, popups::Overlay)>,
    apps: Vec<desktop_entry::Entry>,
    dialogs: Vec<Dialog>,
    children: Vec<Child>,
    control: UnixStream,
    pointer: (WindowId, f64, f64),
    armed: Option<(WindowId, usize)>,
    selected: Option<usize>,
    icons: icons::Icons,
    /// 正在拖动的桌面图标。
    drag: Option<Drag>,
    rename: Option<Rename>,
    paste: Option<clipboard::Paste>,
    /// 真实 Wayland 文件拖放当前是否悬停在回收站上；负责目标高亮与目录刷新边界。
    drop_hover: bool,
    clicked: Option<ItemClick<(WindowId, usize)>>,
    /// 选中标签的单击先等待是否有下一次点击，期间新输入或目录刷新会取消。
    rename_click: Option<(usize, Instant)>,
    task_page: usize,
    home: PathBuf,
    message: String,
    volume: sndvol::Volume,
    network: network::Network,
    usb: usb::Usb,
    /// 每用户桌面设置（任务栏选项、背景色）与它被读取时的修改时间；控制面板改写文件后据此重新应用。
    settings: desktop_settings::Settings,
    settings_stamp: Option<std::time::SystemTime>,
    /// 自动隐藏的任务栏当前是否收起成细线；缺少它无法判断下一次指针进入要不要展开。
    taskbar_hidden: bool,
    /// 指针离开任务栏的时刻；经过 [`HIDE_DELAY`] 仍未回来才收起，避免掠过边缘时闪烁。
    leave_at: Option<Instant>,
    /// 任务栏的悬停提示状态，目标是任务栏命中区序号（时钟、音量、任务按钮）。
    tooltip: classic_ui::Tooltip,
    /// 正显示的提示：它的图层窗口与文字。
    tooltip_window: Option<(WindowId, String)>,
    /// 程序列表刚重读过，需要给新条目载入图标。
    apps_changed: bool,
}

impl Desktop {
    fn create(display: &mut Display, icon_cell: (i32, i32)) -> io::Result<Self> {
        let background = display.create_layer(
            "org.liteos.desktop",
            LayerSettings {
                layer: Layer::Background,
                keyboard: KeyboardInteractivity::OnDemand,
                exclusive_zone: -1,
                ..Default::default()
            },
        )?;
        // 任务栏从一开始就按保存的设置创建，而不是先按默认值出现再改。
        let settings = desktop_settings::Settings::load();
        let taskbar =
            display.create_layer("org.liteos.taskbar", Self::taskbar_layer(&settings, false))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("HOME missing"))?;
        // 登录重连走后台线程；服务器不可达时不能阻塞桌面、任务栏或登录完成。
        std::thread::spawn(|| {
            match network_drive::Store::current().and_then(|store| store.reconnect()) {
                Ok(failures) => {
                    for (letter, error) in failures {
                        eprintln!("网络驱动器 {letter}: {error}");
                    }
                }
                Err(error) => eprintln!("网络驱动器重连：{error}"),
            }
        });
        let control = UnixStream::from(std::io::stdout().as_fd().try_clone_to_owned()?);
        Ok(Self {
            background,
            taskbar,
            menu: None,
            menu_selection: start::Selection::default(),
            overlay: None,
            apps: installed_programs(),
            dialogs: Vec::new(),
            children: Vec::new(),
            control,
            pointer: (background, 0., 0.),
            armed: None,
            selected: None,
            icons: icons::Icons::load(&home, icon_cell),
            drag: None,
            rename: None,
            paste: None,
            drop_hover: false,
            clicked: None,
            rename_click: None,
            task_page: 0,
            home,
            message: String::new(),
            volume: sndvol::Volume::open(),
            network: network::Network::new()?,
            usb: usb::Usb::new()?,
            settings,
            settings_stamp: desktop_settings::Settings::stamp(),
            taskbar_hidden: false,
            leave_at: None,
            tooltip: classic_ui::Tooltip::default(),
            tooltip_window: None,
            apps_changed: true,
        })
    }
    fn toggle_menu(&mut self, display: &mut Display) -> io::Result<()> {
        if let Some(menu) = self.menu.take() {
            display.destroy_window(menu)?;
        } else {
            self.menu_selection = start::Selection::default();
            self.menu = Some(display.create_layer(
                "org.liteos.start",
                LayerSettings {
                    layer: Layer::Overlay,
                    exclusive_zone: -1,
                    keyboard: KeyboardInteractivity::Exclusive,
                    ..Default::default()
                },
            )?);
        }
        Ok(())
    }
    fn dialog(&mut self, display: &mut Display, kind: Kind) -> io::Result<()> {
        if self.dialogs.iter().any(|dialog| dialog.kind == kind) {
            return Ok(());
        }
        self.dialogs.push(Dialog::new(display, kind)?);
        Ok(())
    }
    /// 任务管理器是独立程序（Windows 2000 的 taskmgr.exe），外壳只负责启动；已在运行时再启动一个也无害。
    fn icon_kind(&self, index: usize) -> popups::IconKind {
        match self.icons.items.get(index).map(|icon| &icon.target) {
            Some(icons::Target::Path(path)) if path.is_file() => popups::IconKind::File,
            Some(icons::Target::Path(_)) => popups::IconKind::Path,
            Some(icons::Target::Place(start::Place::MyComputer)) => popups::IconKind::MyComputer,
            Some(icons::Target::Place(start::Place::NetworkPlaces)) => {
                popups::IconKind::NetworkPlaces
            }
            Some(icons::Target::Place(start::Place::RecycleBin)) => popups::IconKind::RecycleBin {
                full: self.icons.trash_full,
            },
            _ => popups::IconKind::Other,
        }
    }

    /// 让资源管理器对桌面上的文件或文件夹执行动词：打开它所在的文件夹、选中它、立即执行。
    fn icon_verb(&mut self, index: usize, verb: &str) {
        let Some(icons::Target::Path(path)) =
            self.icons.items.get(index).map(|icon| icon.target.clone())
        else {
            return;
        };
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let mut command = Command::new("/bin/file-explorer");
        command
            .arg(parent)
            .args(["--select"])
            .arg(name)
            .args(["--verb", verb]);
        self.launch(command);
    }

    fn rename_icon(&mut self, index: usize) {
        let Some(icons::Target::Path(path)) = self.icons.items.get(index).map(|icon| &icon.target)
        else {
            return;
        };
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            return;
        };
        let mut edit = Edit::new(name);
        if edit.text() != name {
            return;
        }
        let operation = match desktop_entry::fileops::Rename::prepare(path.clone()) {
            Ok(operation) => operation,
            Err(error) => {
                self.message = format!("无法重命名：{error}");
                return;
            }
        };
        edit.select_position(0, false);
        edit.select_position(name.len(), true);
        self.selected = Some(index);
        self.clicked = None;
        self.rename = Some(Rename {
            operation,
            edit,
            selecting: false,
        });
        self.message.clear();
    }

    fn refresh_icons(&mut self) {
        let selected = self
            .selected
            .and_then(|index| self.icons.items.get(index))
            .map(|icon| icon.target.clone());
        self.icons.refresh();
        self.selected = selected.and_then(|target| {
            self.icons
                .items
                .iter()
                .position(|icon| icon.target == target)
        });
        self.clicked = None;
        self.rename_click = None;
        self.apps_changed = true;
        if let Some(rename) = &self.rename {
            let visible = self.icons.items.iter().any(|icon| {
                matches!(&icon.target, icons::Target::Path(path) if path == rename.operation.path())
            });
            let error = match rename.operation.verify() {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Some(error.to_string()),
                _ if !visible => Some("正在编辑的项目已不在桌面上，请重新选择。".into()),
                _ => None,
            };
            if let Some(error) = error {
                self.rename = None;
                self.selected = None;
                self.message = format!("无法重命名：{error}");
            }
        }
    }

    fn commit_rename(&mut self) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        match self.icons.rename(&rename.operation, rename.edit.text()) {
            Ok(path) => {
                self.message.clear();
                self.selected = self.icons.items.iter().position(
                    |icon| matches!(&icon.target, icons::Target::Path(current) if *current == path),
                );
                self.apps_changed = true;
            }
            Err(error) => {
                self.message = format!("无法重命名：{error}");
                if error.kind() == io::ErrorKind::NotFound {
                    self.selected = None;
                    self.refresh_icons();
                } else {
                    self.rename = Some(rename);
                }
            }
        }
    }

    fn delete_icon(&mut self, index: usize, verb: &str) {
        if let Some(icons::Target::Path(path)) =
            self.icons.items.get(index).map(|icon| &icon.target)
        {
            let mut command = Command::new("/bin/file-explorer");
            command.arg(verb).arg(path);
            self.launch(command);
            self.clicked = None;
        }
    }

    /// 启动控制面板的某个小程序。
    fn control_panel(&mut self, applet: &str) {
        let mut command = Command::new("/bin/control-panel");
        command.arg(applet);
        self.launch(command);
    }
    fn launch_task_manager(&mut self) {
        self.launch(Command::new("/bin/taskmgr"));
    }
    fn launch(&mut self, mut command: Command) {
        command.stdin(Stdio::null());
        if let Err(error) = self.launch_with_stdin(command) {
            self.message = format!("无法启动程序: {error}");
        }
    }

    fn launch_with_stdin(&mut self, mut command: Command) -> io::Result<()> {
        command.env_remove("WAYLAND_SOCKET").stdout(Stdio::null());
        self.children.push(command.spawn()?);
        Ok(())
    }

    fn recycle_dropped(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        let payload = file_uris::encode(paths, file_uris::MAX_PATHS)?;
        // 标准输入携带完整列表。否则接近 URI 字节上限时，argv 的指针与环境会使 exec 返回 E2BIG。
        let input = MemFd::from_bytes("desktop-recycle", payload.as_bytes())?;
        let mut command = Command::new("/bin/file-explorer");
        command
            .args(["--recycle", "-"])
            .stdin(Stdio::from(input.as_fd().try_clone_to_owned()?));
        self.launch_with_stdin(command)
    }
    /// 在资源管理器中打开位置：路径，或 `::my-computer` 这样的虚拟文件夹名。
    fn files(&mut self, place: impl AsRef<std::ffi::OsStr>) {
        let mut command = Command::new("/bin/file-explorer");
        command.arg(place);
        self.launch(command);
    }
    fn screen(display: &Display, window: WindowId) -> (i32, i32) {
        display
            .configuration(window)
            .map_or((1280, 800), |c| (c.width, c.height))
    }

    /// 在资源管理器里打开桌面图标或菜单对应的位置。
    fn open_place(&mut self, place: start::Place) {
        use start::Place;
        match place {
            Place::Documents => self.files(self.home.clone()),
            Place::MyComputer => self.files("::my-computer"),
            Place::ControlPanel => self.files("::control-panel"),
            Place::NetworkPlaces => self.files("::network-places"),
            Place::NetworkConnections => self.files("::network-connections"),
            Place::RecycleBin => self.files("::recycle-bin"),
        }
    }

    /// 打开桌面图标：系统图标进入对应位置，文件夹在资源管理器中打开，可执行文件启动，其余提示没有关联程序。
    fn open_icon(&mut self, index: usize) {
        let Some(icon) = self.icons.items.get(index) else {
            return;
        };
        match icon.target.clone() {
            icons::Target::Place(place) => self.open_place(place),
            icons::Target::Path(path) if path.is_dir() => self.files(path),
            icons::Target::Path(path) => match desktop_entry::open(&path) {
                Ok(command) => self.launch(command),
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                    self.icon_verb(index, "open-with")
                }
                Err(error) => self.message = format!("无法打开 {}: {error}", icon.label),
            },
        }
    }

    fn network_status(&mut self, index: u32) {
        let Some(link) = self.network.link(index) else {
            self.message = "网络连接已移除".into();
            return;
        };
        let mut command = Command::new("/bin/control-panel");
        command.args(["network", &link.interface]);
        self.launch(command);
    }

    fn offer_icon(&mut self, display: &mut Display, index: usize, cut: bool) {
        let Some(icons::Target::Path(path)) = self.icons.items.get(index).map(|icon| &icon.target)
        else {
            return;
        };
        let result = clipboard::offer(display, std::slice::from_ref(path), cut, None);
        match result {
            Ok(true) => self.message.clear(),
            Ok(false) => self.message = "剪贴板不可用".into(),
            Err(error) => self.message = format!("无法将项目放入剪贴板：{error}"),
        }
    }

    fn begin_paste(&mut self, display: &mut Display) {
        if self.paste.is_none() {
            match clipboard::Paste::begin(display) {
                Ok(Some(paste)) => {
                    self.paste = Some(paste);
                    self.message = "正在粘贴…".into();
                }
                Ok(None) => {}
                Err(error) => self.message = format!("无法粘贴：{error}"),
            }
        }
    }

    fn poll_paste(&mut self, display: &mut Display) -> bool {
        let Some(paste) = self.paste.take() else {
            return false;
        };
        match paste.poll(display, &self.home) {
            Ok(Some(waiting)) => {
                self.paste = Some(waiting);
                false
            }
            Ok(None) => {
                self.message.clear();
                true
            }
            Err(error) => {
                self.message = format!("无法粘贴：{error}");
                true
            }
        }
    }

    fn perform(&mut self, display: &mut Display, action: popups::Action) -> io::Result<()> {
        use popups::Action;
        match action {
            Action::MinimizeAll => {
                let ids: Vec<_> = display.toplevels().map(|(id, _)| id).collect();
                for id in ids {
                    display.control_toplevel(id, ToplevelRequest::Minimize(true))?;
                }
            }
            Action::TaskManager => self.launch_task_manager(),
            Action::TaskbarProperties => self.control_panel("taskbar"),
            Action::SystemProperties => self.control_panel("system"),
            Action::OpenPlace(place) => self.open_place(place),
            Action::IconVerb(index, verb) => self.icon_verb(index, verb),
            Action::DisplayProperties => self.control_panel("display"),
            Action::VolumeProperties => self.launch(Command::new("/bin/sndvol")),
            Action::NetworkStatus(index) => self.network_status(index),
            Action::DisableNetwork(index) => {
                if let Err(error) = self.network.disable(index) {
                    self.message = format!("无法禁用连接：{error}");
                }
            }
            Action::UsbDevices => {
                let mut command = Command::new("/bin/devmgmt");
                command.arg("--eject");
                self.launch(command);
            }
            Action::OpenIcon(index) => self.open_icon(index),
            Action::RenameIcon(index) => self.rename_icon(index),
            Action::DeleteIcon(index) => self.delete_icon(index, "--delete"),
            Action::FileSelection { index, cut } => self.offer_icon(display, index, cut),
            Action::PasteFiles => self.begin_paste(display),
            Action::EmptyRecycleBin => {
                let mut command = Command::new("/bin/file-explorer");
                command.arg("--empty-recycle-bin");
                self.launch(command);
            }
            Action::NewFolder => match self.icons.new_folder() {
                Ok(path) => {
                    self.refresh_icons();
                    if let Some(index) = self
                        .icons
                        .items
                        .iter()
                        .position(|icon| matches!(&icon.target, icons::Target::Path(current) if *current == path))
                    {
                        self.rename_icon(index);
                    }
                }
                Err(error) => self.message = error.to_string(),
            },
            Action::ArrangeIcons => match self.icons.auto_arrange() {
                Ok(()) => self.refresh_icons(),
                Err(error) => self.message = error.to_string(),
            },
            Action::Refresh => {
                self.apps = installed_programs();
                self.refresh_icons();
            }
            Action::Window(id, request) => display.control_toplevel(id, request)?,
        }
        Ok(())
    }

    /// 执行开始菜单的叶子动作；先关闭菜单，使弹出的窗口拿到键盘焦点。
    fn start_action(&mut self, display: &mut Display, action: start::Action) -> io::Result<()> {
        use start::Action;
        self.toggle_menu(display)?;
        match action {
            Action::Application(index) => {
                if let Some(app) = self.apps.get(index) {
                    self.launch(app.command());
                }
            }
            Action::Files(place) => self.open_place(place),
            Action::Tasks => self.launch_task_manager(),
            Action::Taskbar => self.control_panel("taskbar"),
            Action::About => {
                let mut command = Command::new("/bin/control-panel");
                command.arg("winver");
                self.launch(command);
            }
            Action::Run => self.dialog(display, Kind::Run)?,
            Action::Lock => self.control.write_all(&[2])?,
            Action::Logoff => {
                // 注销音效播完再退出，否则进程退出会截断声音。
                let _ = sound::play("logoff.wav").join();
                std::process::exit(0)
            }
            Action::Shutdown => self.control.write_all(&[4])?,
        }
        Ok(())
    }
    fn menu_geometry(&self, c: classic_ui::Configuration) -> (Vec<start::Row>, start::Geometry) {
        let rows = start::rows(&self.apps);
        let geometry = start::geometry(
            &rows,
            (c.width, c.height),
            TASKBAR_HEIGHT,
            Some(self.menu_selection.top),
        );
        (rows, geometry)
    }
    /// 开始菜单的键盘导航：上下在当前层移动，右进入子菜单，左退出，回车执行。
    fn menu_key(&mut self, display: &mut Display, code: u32) -> io::Result<()> {
        let Some(menu) = self.menu else {
            return Ok(());
        };
        let Some(c) = display.configuration(menu) else {
            return Ok(());
        };
        let (rows, _) = self.menu_geometry(c);
        let selection = &mut self.menu_selection;
        let step = |index: usize, count: usize, down: bool| {
            if down {
                (index + 1) % count
            } else {
                (index + count - 1) % count
            }
        };
        match (code, selection.sub) {
            (1 | 125 | 126, _) => self.toggle_menu(display)?,
            (103 | 108, None) => selection.top = step(selection.top, rows.len(), code == 108),
            (103 | 108, Some(sub)) => {
                let count = rows[selection.top].submenu.len();
                selection.sub = Some(step(sub, count, code == 108));
            }
            (106, None) if !rows[selection.top].submenu.is_empty() => selection.sub = Some(0),
            (105, Some(_)) => selection.sub = None,
            (28, sub) => {
                let row = &rows[selection.top];
                let action = match sub {
                    Some(sub) => row.submenu[sub].action,
                    None => row.action,
                };
                if let Some(action) = action {
                    self.start_action(display, action)?;
                } else if sub.is_none() && !row.submenu.is_empty() {
                    selection.sub = Some(0);
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn event(
        &mut self,
        display: &mut Display,
        art: &mut paint::Art,
        event: Event,
        pending: &mut HashSet<WindowId>,
    ) -> io::Result<()> {
        let window = event.window;
        let motion = matches!(
            event.kind,
            EventKind::PointerEnter { .. } | EventKind::PointerMove { .. }
        );
        if matches!(
            event.kind,
            EventKind::Button { pressed: true, .. }
                | EventKind::Focus(false)
                | EventKind::Wheel { .. }
                | EventKind::FileDragOver { .. }
        ) || matches!(event.kind, EventKind::Key(key) if key.pressed)
        {
            self.rename_click = None;
        }
        if window == self.background
            && (matches!(event.kind, EventKind::Focus(false))
                || matches!(event.kind, EventKind::Key(key) if key.pressed && key.code == 1))
            && self.drag.take().is_some()
        {
            self.armed = None;
            self.clicked = None;
            return Ok(());
        }
        match &event.kind {
            EventKind::FileDragOver { x, y } => {
                let accept = if window == self.background
                    && let Some(configuration) = display.configuration(window)
                {
                    art.recycle_target(&self.icons, configuration.scale, (*x, *y))?
                } else {
                    false
                };
                if std::mem::replace(&mut self.drop_hover, accept) != accept {
                    pending.insert(self.background);
                }
                return display.accept_file_drag(accept);
            }
            EventKind::FileDragLeave => {
                if std::mem::take(&mut self.drop_hover) {
                    pending.insert(self.background);
                }
                return Ok(());
            }
            EventKind::FilesDropped(paths) => {
                if window == self.background
                    && let Err(error) = self.recycle_dropped(paths)
                {
                    self.message = format!("无法移入回收站：{error}");
                    pending.insert(window);
                }
                return Ok(());
            }
            _ => {}
        }
        if window == self.taskbar && self.settings.auto_hide {
            match event.kind {
                EventKind::PointerEnter { .. } => {
                    self.leave_at = None;
                    if self.taskbar_hidden {
                        self.taskbar_hidden = false;
                        self.apply_taskbar(display)?;
                    }
                }
                EventKind::PointerLeave => self.leave_at = Some(Instant::now()),
                _ => {}
            }
        }
        if self.overlay.as_ref().is_some_and(|(w, _)| *w == window) {
            if motion {
                pending.insert(window);
            }
            return self.overlay_event(display, event);
        }
        if window == self.taskbar {
            match event.kind {
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    let hit = display.configuration(window).and_then(|c| {
                        paint::areas(self, display, window, c).iter().position(|r| {
                            x >= f64::from(r.x)
                                && x < f64::from(r.right())
                                && y >= f64::from(r.y)
                                && y < f64::from(r.bottom())
                        })
                    });
                    self.tooltip.hover(hit);
                }
                EventKind::PointerLeave => self.tooltip.hover(None),
                EventKind::Button { pressed: true, .. } => self.tooltip.dismiss(),
                _ => {}
            }
        }
        if let Some(index) = self.dialogs.iter().position(|d| d.window == window) {
            if motion {
                pending.insert(window);
            }
            match self.dialogs[index].event(display, event)? {
                dialogs::Action::Close => {
                    display.destroy_window(window)?;
                    self.dialogs.remove(index);
                }
                dialogs::Action::Run(text) => {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", &text]);
                    self.launch(command);
                    display.destroy_window(window)?;
                    self.dialogs.remove(index);
                }
                dialogs::Action::None => {}
            }
            return Ok(());
        }
        if window == self.background
            && let Some(rename) = &mut self.rename
            && let Some(index) = self.icons.items.iter().position(
                |icon| matches!(&icon.target, icons::Target::Path(path) if path == rename.operation.path()),
            )
            && let Some(configuration) = display.configuration(window)
        {
            let icon = &self.icons.items[index];
            let height = art
                .desktop_icon_bitmap(icon, self.icons.trash_full)
                .map_or(32, classic_ui::Bitmap::height);
            let rect = classic_ui::large_icon_edit_rect(self.icons.rect(index), height);
            match event.kind {
                EventKind::Key(key) if key.pressed => {
                    match key.code {
                        1 => {
                            self.rename = None;
                            self.message.clear();
                        }
                        28 => self.commit_rename(),
                        _ => {
                            rename.edit.key(key);
                        }
                    }
                    return Ok(());
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } if (f64::from(rect.x)..f64::from(rect.right())).contains(&self.pointer.1)
                    && (f64::from(rect.y)..f64::from(rect.bottom())).contains(&self.pointer.2) =>
                {
                    let position = art.icon_edit_hit(
                        configuration.scale,
                        rect,
                        &rename.edit,
                        self.pointer.1 as i32,
                    )?;
                    rename
                        .edit
                        .select_position(position, display.keyboard_modifiers().shift);
                    rename.selecting = true;
                    return Ok(());
                }
                EventKind::PointerMove { x, y } if rename.selecting => {
                    self.pointer = (window, x, y);
                    let position =
                        art.icon_edit_hit(configuration.scale, rect, &rename.edit, x as i32)?;
                    rename.edit.select_position(position, true);
                    pending.insert(window);
                    return Ok(());
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } if rename.selecting => {
                    rename.selecting = false;
                    return Ok(());
                }
                EventKind::Button { pressed: true, .. } => {
                    self.commit_rename();
                    if self.rename.is_some() {
                        return Ok(());
                    }
                }
                EventKind::Focus(false) => self.commit_rename(),
                EventKind::PointerLeave => rename.selecting = false,
                _ => {}
            }
        }
        match event.kind {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (window, x, y);
                if let Some(drag) = &mut self.drag
                    && window == self.background
                    && drag.pos != (x as i32, y as i32)
                {
                    drag.pos = (x as i32, y as i32);
                    let start = self.icons.rect(drag.index);
                    drag.moved |= (drag.pos.0 - drag.grab.0 - start.x).abs() > 4
                        || (drag.pos.1 - drag.grab.1 - start.y).abs() > 4;
                    if drag.moved {
                        if let Some(configuration) = display.configuration(window) {
                            drag.recycle = matches!(
                                self.icons.items[drag.index].target,
                                icons::Target::Path(_)
                            ) && art.recycle_target(
                                &self.icons,
                                configuration.scale,
                                (x, y),
                            )?;
                        }
                        pending.insert(window);
                    }
                }
                if Some(window) == self.menu
                    && let Some(c) = display.configuration(window)
                {
                    let (_, geometry) = self.menu_geometry(c);
                    if let Some(selection) = start::hit(&geometry, (x as i32, y as i32))
                        && self.menu_selection != selection
                    {
                        self.menu_selection = selection;
                        pending.insert(window);
                    }
                }
            }
            EventKind::PointerLeave => self.armed = None,
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                let c = match display.configuration(window) {
                    Some(c) => c,
                    None => return Ok(()),
                };
                if Some(window) == self.menu {
                    if !pressed {
                        let (rows, geometry) = self.menu_geometry(c);
                        let point = (self.pointer.1 as i32, self.pointer.2 as i32);
                        match start::hit(&geometry, point) {
                            Some(start::Selection { top, sub }) => {
                                let row = &rows[top];
                                let action = sub.map_or(row.action, |sub| row.submenu[sub].action);
                                if let Some(action) = action {
                                    self.start_action(display, action)?;
                                }
                            }
                            None if !(geometry.panel.x..geometry.panel.right())
                                .contains(&point.0)
                                || !(geometry.panel.y..geometry.panel.bottom())
                                    .contains(&point.1) =>
                            {
                                self.toggle_menu(display)?
                            }
                            None => {}
                        }
                    }
                    return Ok(());
                }
                let icon_hit = if window == self.background {
                    let expanded = self
                        .selected
                        .filter(|_| display.keyboard_focus() == Some(window));
                    art.icon_at(
                        &self.icons,
                        c.scale,
                        (self.pointer.1, self.pointer.2),
                        expanded,
                    )?
                } else {
                    None
                };
                let hit = if window == self.background {
                    icon_hit.map(|(index, _)| index)
                } else {
                    paint::areas(self, display, window, c).iter().position(|r| {
                        self.pointer.1 >= f64::from(r.x)
                            && self.pointer.1 < f64::from(r.right())
                            && self.pointer.2 >= f64::from(r.y)
                            && self.pointer.2 < f64::from(r.bottom())
                    })
                };
                let point = (self.pointer.1 as i32, self.pointer.2 as i32);
                let drag = (!pressed).then(|| self.drag.take()).flatten();
                if pressed {
                    self.armed = hit.map(|i| (window, i));
                    if window == self.background {
                        if hit.is_none() {
                            self.clicked = None;
                        }
                        self.drag = hit.map(|index| {
                            let rect = self.icons.rect(index);
                            let pos = (self.pointer.1 as i32, self.pointer.2 as i32);
                            Drag {
                                index,
                                grab: (pos.0 - rect.x, pos.1 - rect.y),
                                pos,
                                moved: false,
                                recycle: false,
                                was_selected: self.selected == Some(index),
                            }
                        });
                        self.selected = hit;
                    }
                } else if let Some(drag) = drag.as_ref().filter(|d| d.moved) {
                    self.armed = None;
                    self.clicked = None;
                    if window == self.background && drag.recycle {
                        self.delete_icon(drag.index, "--recycle");
                    } else if window == self.background {
                        let screen = Self::screen(display, self.background);
                        let rect = self.icons.rect(drag.index);
                        let center = (
                            drag.pos.0 - drag.grab.0 + rect.width / 2,
                            drag.pos.1 - drag.grab.1 + rect.height / 2,
                        );
                        if let Err(error) = self.icons.drop_at(drag.index, center, screen) {
                            self.message = error.to_string();
                        }
                    }
                } else if let Some((_, index)) = self
                    .armed
                    .take()
                    .filter(|(w, i)| *w == window && Some(*i) == hit)
                {
                    if window == self.taskbar {
                        match index {
                            0 => self.toggle_menu(display)?,
                            1 => {
                                let ids: Vec<_> = display.toplevels().map(|(id, _)| id).collect();
                                for id in ids {
                                    display
                                        .control_toplevel(id, ToplevelRequest::Minimize(true))?;
                                }
                            }
                            2 => self.open_place(start::Place::Documents),
                            3 => self.task_page = self.task_page.saturating_sub(1),
                            4 => self.task_page += 1,
                            paint::CLOCK => {
                                let now = Instant::now();
                                if self.clicked.as_ref().is_some_and(|previous| {
                                    previous.follows((window, paint::CLOCK), point, now)
                                }) {
                                    let mut command = Command::new("/bin/control-panel");
                                    command.arg("datetime");
                                    self.launch(command);
                                    self.clicked = None;
                                } else {
                                    self.clicked =
                                        Some(ItemClick::new((window, paint::CLOCK), point, now));
                                }
                            }
                            paint::NETWORK => match self.network.current(&self.settings) {
                                Some(link) if link.state != network_config::LinkState::Disabled => {
                                    self.network_status(link.index);
                                }
                                Some(_) => self.open_place(start::Place::NetworkConnections),
                                None => {}
                            },
                            paint::USB => self.perform(display, popups::Action::UsbDevices)?,
                            paint::VOLUME => {
                                let rect = paint::areas(self, display, window, c)[paint::VOLUME];
                                let center = rect.x + rect.width / 2;
                                self.open_volume_panel(display, art, center)?
                            }
                            n => {
                                let ids = paint::task_ids(self, display, c.width);
                                if let Some(id) = ids.get(n - paint::TASKS_FROM) {
                                    let active = display
                                        .toplevels()
                                        .find(|(target, _)| target == id)
                                        .is_some_and(|(_, info)| info.active && !info.minimized);
                                    display.control_toplevel(
                                        *id,
                                        if active {
                                            ToplevelRequest::Minimize(true)
                                        } else {
                                            ToplevelRequest::Activate
                                        },
                                    )?;
                                }
                            }
                        }
                    } else if window == self.background {
                        self.selected = Some(index);
                        let now = Instant::now();
                        if self
                            .clicked
                            .as_ref()
                            .is_some_and(|previous| previous.follows((window, index), point, now))
                        {
                            self.open_icon(index);
                            self.clicked = None;
                        } else {
                            self.clicked = Some(ItemClick::new((window, index), point, now));
                            let modifiers = display.keyboard_modifiers();
                            if drag.is_some_and(|drag| drag.was_selected && drag.index == index)
                                && icon_hit == Some((index, true))
                                && !modifiers.shift
                                && !modifiers.control
                                && !modifiers.alt
                                && !modifiers.super_key
                                && let Some(icon) = self.icons.items.get(index)
                                && matches!(icon.target, icons::Target::Path(_))
                            {
                                self.rename_click = Some((index, now + ITEM_DOUBLE_CLICK));
                            }
                        }
                    }
                }
            }
            EventKind::Key(key) if key.pressed => {
                if Some(window) == self.menu {
                    self.menu_key(display, key.code)?;
                } else {
                    match key.code {
                        125 | 126 => self.toggle_menu(display)?,
                        1 if key.modifiers.control && key.modifiers.shift => {
                            self.launch_task_manager()
                        }
                        1 if key.modifiers.control => self.toggle_menu(display)?,
                        19 if key.modifiers.super_key => self.dialog(display, Kind::Run)?,
                        103 | 105 | 106 | 108
                            if window == self.background
                                && self.drag.is_none()
                                && !key.modifiers.shift
                                && !key.modifiers.control
                                && !key.modifiers.alt
                                && !key.modifiers.super_key =>
                        {
                            let direction = match key.code {
                                103 => (0, -1),
                                105 => (-1, 0),
                                106 => (1, 0),
                                _ => (0, 1),
                            };
                            let (width, height) = Self::screen(display, window);
                            self.selected = self.icons.step(
                                self.selected,
                                direction,
                                (width, height - TASKBAR_HEIGHT),
                            );
                            self.clicked = None;
                        }
                        68 | 127 | 139
                            if window == self.background
                                && self.drag.is_none()
                                && !key.repeat
                                && !key.modifiers.control
                                && !key.modifiers.alt
                                && !key.modifiers.super_key
                                && key.modifiers.shift == (key.code == 68) =>
                        {
                            self.context_menu(display, art, window, true)?;
                        }
                        45..=47
                            if window == self.background
                                && !key.repeat
                                && key.modifiers.control
                                && !key.modifiers.shift
                                && !key.modifiers.alt
                                && !key.modifiers.super_key =>
                        {
                            if key.code == 47 {
                                self.begin_paste(display);
                            } else if let Some(index) = self.selected {
                                self.offer_icon(display, index, key.code == 45);
                            }
                        }
                        60 if window == self.background
                            && !key.modifiers.alt
                            && !key.modifiers.control =>
                        {
                            if let Some(index) = self.selected {
                                self.rename_icon(index);
                            }
                        }
                        111 if window == self.background
                            && !key.repeat
                            && !key.modifiers.alt
                            && !key.modifiers.super_key
                            && !key.modifiers.control =>
                        {
                            if let Some(index) = self.selected {
                                let verb = if key.modifiers.shift {
                                    "--delete-permanently"
                                } else {
                                    "--delete"
                                };
                                self.delete_icon(index, verb);
                            }
                        }
                        28 if window == self.background => {
                            if let Some(index) = self.selected {
                                self.open_icon(index);
                            }
                        }
                        62 if key.modifiers.alt && window == self.background => {
                            self.control.write_all(&[4])?
                        }
                        _ => {}
                    }
                }
            }
            EventKind::Button {
                code: 0x111,
                pressed: false,
                ..
            } if Some(window) != self.menu => self.context_menu(display, art, window, false)?,
            EventKind::Close if Some(window) == self.menu => self.toggle_menu(display)?,
            _ => {}
        }
        Ok(())
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let mut art = paint::Art::load()?;
    let mut state = Desktop::create(&mut display, art.icon_cell())?;
    // Windows 2000 的 savedump.exe：上次以内核崩溃结束时提示“系统已从严重错误中恢复”，否则立即退出。
    state.launch(Command::new("/bin/savedump"));
    let mut dirty = true;
    let mut ready = false;
    let mut presented = HashSet::new();
    let mut minute = 0;
    let mut last_unix_minute = 0;
    let mut revision = 0;
    // 外部安装器会写入 XDG 应用目录；每秒刷新一次，避免每个输入事件都扫描完整应用树。
    let mut application_poll = Instant::now();
    // 持续移动鼠标时也按普通空闲循环的频率查目录，避免每个输入批次都查询文件系统。
    let mut icon_poll = Instant::now();
    let mut pending = HashSet::new();
    loop {
        // 1. 同批已到达的输入先按顺序交付，再绘制最终状态；逐事件重画全屏会让滑块追赶旧坐标。
        // 2. 每轮至多处理 64 项，持续输入时仍给绘制、外部状态和会话回收执行机会。
        for index in 0..64 {
            let timeout = if index == 0 {
                state
                    .rename_click
                    .map_or(Duration::from_millis(100), |(_, deadline)| {
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(100))
                    })
            } else {
                Duration::ZERO
            };
            let event = {
                use linux_uapi::unix::{PollEvents, PollFd};
                let mut source = art
                    .picture_notification()
                    .map(|fd| PollFd::new(fd, PollEvents::READ));
                display.next_event_with_sources(Some(timeout), source.as_mut_slice())?
            };
            let Some(event) = event else {
                break;
            };
            if matches!(event.kind, EventKind::FrameReady { .. }) {
                presented.insert(event.window);
            }
            // 指针移动由命中的交互单独失效，空白背景与托盘提示计时不要求重画桌面。
            if !matches!(
                event.kind,
                EventKind::FrameReady { .. }
                    | EventKind::BufferAvailable
                    | EventKind::PointerEnter { .. }
                    | EventKind::PointerMove { .. }
                    | EventKind::FileDragOver { .. }
                    | EventKind::FileDragLeave
                    | EventKind::FilesDropped(_)
            ) {
                dirty = true;
            }
            state.event(&mut display, &mut art, event, &mut pending)?;
        }
        dirty |= state.volume.poll();
        dirty |= state.poll_paste(&mut display);
        // 菜单映射、键盘 enter 和剪贴板 offer 可分批到达，按本轮处理后的状态更新可用性。
        if let Some((window, popups::Overlay::Menu(menu))) = &mut state.overlay
            && let Some(item) = menu
                .items
                .iter_mut()
                .find(|item| matches!(item.action, popups::Action::PasteFiles))
        {
            let disabled = state.paste.is_some() || !clipboard::can_paste(&display);
            if item.disabled != disabled {
                item.disabled = disabled;
                pending.insert(*window);
            }
        }
        match state.network.poll() {
            Ok(changed) => dirty |= changed,
            Err(error) => {
                state.message = format!("无法禁用连接：{error}");
                dirty = true;
            }
        }
        dirty |= state.usb.poll();
        if state.icons.poll_trash() {
            pending.insert(state.background);
        }
        dirty |= state.poll_settings(&mut display)?;
        if art.update_wallpaper(state.settings.wallpaper.as_deref())? {
            pending.insert(state.background);
        }
        state.tick_auto_hide(&mut display)?;
        if application_poll.elapsed() >= Duration::from_secs(1) {
            application_poll = Instant::now();
            let apps = installed_programs();
            if apps != state.apps {
                state.apps = apps;
                if state.overlay.is_none() && state.drag.is_none() && !state.drop_hover {
                    state.refresh_icons();
                }
                state.apps_changed = true;
            }
        }
        if icon_poll.elapsed() >= Duration::from_millis(100)
            && state.overlay.is_none()
            && state.drag.is_none()
            && !state.drop_hover
        {
            icon_poll = Instant::now();
            if state.icons.changed() {
                state.refresh_icons();
            }
        }
        if let Some((index, deadline)) = state.rename_click
            && deadline <= Instant::now()
        {
            state.rename_click = None;
            if display.keyboard_focus() == Some(state.background) && state.selected == Some(index) {
                state.rename_icon(index);
                pending.insert(state.background);
            }
        }
        if std::mem::take(&mut state.apps_changed) {
            art.register(&state.apps, &state.icons.items);
            dirty = true;
        }
        state.sync_tooltip(&mut display, &mut art)?;
        if dirty {
            state.sync_task_rectangles(&mut display)?;
        }
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        // 每分钟重读一次 /etc/localtime：控制面板改了时区后，长驻的外壳据此刷新时钟。
        if unix / 60 != last_unix_minute {
            last_unix_minute = unix / 60;
            if let Err(error) = calendar::zone::reload() {
                eprintln!("desktop: timezone reload failed: {error}");
            }
        }
        let now = (calendar::zone::to_local(unix) / 60) as u64;
        let current_revision = display.toplevel_revision();
        if now != minute || revision != current_revision {
            minute = now;
            revision = current_revision;
            dirty = true;
        }
        use linux_uapi::unix::{PollEvents, PollFd, poll};
        let mut fd = [PollFd::new(state.control.as_fd(), PollEvents::READ)];
        if poll(&mut fd, Some(Duration::ZERO))? != 0 {
            let mut byte = [0];
            if state.control.read(&mut byte)? == 0 {
                return Ok(());
            }
            if byte[0] != 5 {
                return Err("invalid logon session request".into());
            }
            state.launch_task_manager();
            dirty = true;
        }
        state
            .children
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        let ids: Vec<_> = [state.background, state.taskbar]
            .into_iter()
            .chain(state.menu)
            .chain(state.overlay.as_ref().map(|(w, _)| *w))
            .chain(state.dialogs.iter().map(|d| d.window))
            .chain(state.tooltip_window.as_ref().map(|(w, _)| *w))
            .collect();
        if dirty {
            pending.extend(ids.iter().copied());
            dirty = false;
        }
        pending.retain(|id| ids.contains(id));
        for id in ids {
            if !pending.contains(&id) || !display.ready_to_draw(id) {
                continue;
            }
            let c = display
                .configuration(id)
                .ok_or("missing desktop configure")?;
            if id == state.background {
                display.paint_frame(id, |pixels, configuration, preserved, display| {
                    paint::background_frame(
                        &state,
                        display,
                        configuration,
                        &mut art,
                        pixels,
                        preserved,
                    )
                })?;
            } else {
                let pixels = paint::frame(&state, &display, id, c, &mut art, minute)?;
                display.present(id, &pixels)?;
            }
            pending.remove(&id);
        }
        if !ready && presented.contains(&state.background) && presented.contains(&state.taskbar) {
            state.control.write_all(&[1])?;
            ready = true;
            eprintln!("desktop: ready");
            let _ = sound::play("logon.wav");
        }
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("desktop: error: {error}");
            ExitCode::FAILURE
        }
    }
}
