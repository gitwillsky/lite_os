//! 用户身份运行的桌面外壳；仅持有 logon 委派的 Shell 连接。
extern crate liteos_runtime as _;
mod dialogs;
mod icons;
mod paint;
mod popups;
mod start;
use classic_ui::{
    Display, Event, EventKind, KeyboardInteractivity, Layer, LayerSettings, ToplevelRequest,
    WindowId, layer_anchor,
};
use dialogs::{Dialog, Kind};
use std::{
    io::{self, Read, Write},
    os::{
        fd::AsFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    time::{Duration, Instant},
};

/// 任务栏高度（逻辑像素）；开始菜单贴在它上方。
const TASKBAR_HEIGHT: i32 = 28;

/// 开始菜单"程序"里的条目：已安装程序里除设置程序（属于控制面板）之外的前 12 个。
fn installed_programs() -> Vec<desktop_entry::Entry> {
    let mut apps = desktop_entry::load(std::path::Path::new("/usr/share/applications"));
    apps.retain(|app| !app.in_category("Settings"));
    apps.truncate(12);
    apps
}

/// 一次图标拖动：按下时指针在图标内的偏移、当前指针位置，以及是否已移动足够远（区分点击与拖动）。
struct Drag {
    index: usize,
    grab: (i32, i32),
    pos: (i32, i32),
    moved: bool,
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
    clicked: Option<(usize, Instant)>,
    task_page: usize,
    home: PathBuf,
    message: String,
    volume: sndvol::Volume,
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
}

/// 自动隐藏的任务栏收起后露出的细线高度。
const HIDDEN_HEIGHT: i32 = 2;
/// 指针离开多久后收起自动隐藏的任务栏。
const HIDE_DELAY: Duration = Duration::from_millis(400);
impl Desktop {
    fn create(display: &mut Display) -> io::Result<Self> {
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
            icons: icons::Icons::load(&home),
            drag: None,
            clicked: None,
            task_page: 0,
            home,
            message: String::new(),
            volume: sndvol::Volume::open(),
            settings,
            settings_stamp: desktop_settings::Settings::stamp(),
            taskbar_hidden: false,
            leave_at: None,
            tooltip: classic_ui::Tooltip::default(),
            tooltip_window: None,
        })
    }
    /// 任务栏图层的设置：层次（总在最前）、高度与独占区（自动隐藏时收起并释放独占区）。
    fn taskbar_layer(settings: &desktop_settings::Settings, hidden: bool) -> LayerSettings {
        let height = if settings.auto_hide && hidden {
            HIDDEN_HEIGHT
        } else {
            TASKBAR_HEIGHT
        };
        LayerSettings {
            layer: if settings.always_on_top {
                Layer::Top
            } else {
                Layer::Bottom
            },
            height: height as u32,
            anchor: layer_anchor::LEFT | layer_anchor::RIGHT | layer_anchor::BOTTOM,
            // 自动隐藏时窗口使用整个屏幕，任务栏浮在上面；否则为它保留一条。
            exclusive_zone: if settings.auto_hide {
                0
            } else {
                TASKBAR_HEIGHT
            },
            keyboard: KeyboardInteractivity::OnDemand,
            ..Default::default()
        }
    }

    /// 按当前设置重新配置任务栏图层。
    fn apply_taskbar(&mut self, display: &mut Display) -> io::Result<()> {
        display.configure_layer(
            self.taskbar,
            Self::taskbar_layer(&self.settings, self.taskbar_hidden),
        )
    }

    /// 设置文件变了就重读并应用；返回是否需要重画。
    fn poll_settings(&mut self, display: &mut Display) -> io::Result<bool> {
        let stamp = desktop_settings::Settings::stamp();
        if stamp == self.settings_stamp {
            return Ok(false);
        }
        self.settings_stamp = stamp;
        self.settings = desktop_settings::Settings::load();
        self.taskbar_hidden = false;
        self.leave_at = None;
        self.apply_taskbar(display)?;
        Ok(true)
    }

    /// 把每个任务按钮的位置告诉 compositor，最小化与还原动画用它作端点。
    fn sync_task_rectangles(&self, display: &mut Display) -> io::Result<()> {
        let Some(c) = display.configuration(self.taskbar) else {
            return Ok(());
        };
        let rects = paint::areas(self, display, self.taskbar, c);
        let ids = paint::task_ids(self, display, c.width);
        for (index, id) in ids.into_iter().enumerate() {
            if let Some(rect) = rects.get(paint::TASKS_FROM + index) {
                display.set_toplevel_rectangle(id, self.taskbar, *rect)?;
            }
        }
        Ok(())
    }

    /// 某个任务栏命中区的提示文字；没有提示的区域为 `None`。
    fn tooltip_text(&self, display: &Display, target: usize) -> Option<String> {
        match target {
            paint::CLOCK => {
                let days = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs() as i64
                    / 86_400;
                let date = calendar::civil_from_days(days);
                let weekday = ["日", "一", "二", "三", "四", "五", "六"]
                    [calendar::weekday(days) as usize % 7];
                Some(format!(
                    "{}年{}月{}日 星期{weekday}",
                    date.year, date.month, date.day
                ))
            }
            paint::VOLUME => Some(match self.volume.value {
                Some(value) if value.muted => "音量：静音".to_owned(),
                Some(value) => format!("音量：{}%", value.left.max(value.right)),
                None => "音量".to_owned(),
            }),
            n if n >= paint::TASKS_FROM => {
                let width = Self::screen(display, self.background).0;
                let ids = paint::task_ids(self, display, width);
                let (_, info) = display
                    .toplevels()
                    .find(|(id, _)| Some(id) == ids.get(n - paint::TASKS_FROM))?;
                Some(info.title.clone())
            }
            _ => None,
        }
    }

    /// 让提示图层与悬停状态一致：到点显示，换了目标或指针离开就撤掉。
    fn sync_tooltip(&mut self, display: &mut Display, art: &mut paint::Art) -> io::Result<()> {
        let wanted = self
            .tooltip
            .visible()
            .and_then(|target| Some((target, self.tooltip_text(display, target)?)));
        let current = self.tooltip_window.as_ref().map(|(_, text)| text.clone());
        if wanted.as_ref().map(|(_, text)| text) == current.as_ref() {
            return Ok(());
        }
        if let Some((window, _)) = self.tooltip_window.take() {
            display.destroy_window(window)?;
        }
        let Some((_, text)) = wanted else {
            return Ok(());
        };
        let (screen_width, _) = Self::screen(display, self.background);
        let scale = display.configuration(self.taskbar).map_or(1, |c| c.scale);
        let (width, height) = art.tooltip_size(&text, scale)?;
        let x = (self.pointer.1 as i32).min(screen_width - width);
        let window = display.create_layer(
            "org.liteos.tooltip",
            LayerSettings {
                layer: Layer::Overlay,
                width: width as u32,
                height: height as u32,
                anchor: layer_anchor::LEFT | layer_anchor::BOTTOM,
                // 位置：左边距取指针横坐标，下边距让它落在任务栏之上。
                margin: [0, 0, TASKBAR_HEIGHT + 4, x.max(0)],
                exclusive_zone: -1,
                ..Default::default()
            },
        )?;
        self.tooltip_window = Some((window, text));
        Ok(())
    }

    /// 自动隐藏：指针进入细线就展开；离开一小会儿且没有菜单或弹出层占着任务栏时收起。
    fn tick_auto_hide(&mut self, display: &mut Display) -> io::Result<()> {
        if !self.settings.auto_hide {
            return Ok(());
        }
        if self.leave_at.is_some_and(|at| {
            at.elapsed() >= HIDE_DELAY && self.menu.is_none() && self.overlay.is_none()
        }) {
            self.leave_at = None;
            self.taskbar_hidden = true;
            self.apply_taskbar(display)?;
        }
        Ok(())
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
        command
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        match command.spawn() {
            Ok(child) => self.children.push(child),
            Err(error) => self.message = format!("无法启动程序: {error}"),
        }
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

    fn close_overlay(&mut self, display: &mut Display) -> io::Result<()> {
        if let Some((window, overlay)) = self.overlay.take() {
            if matches!(overlay, popups::Overlay::Volume(_)) {
                self.volume.cancel();
            }
            display.destroy_window(window)?;
        }
        Ok(())
    }

    fn open_overlay(&mut self, display: &mut Display, overlay: popups::Overlay) -> io::Result<()> {
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

    /// 在资源管理器里打开桌面图标/菜单对应的位置；回收站目录不存在时先创建。
    fn open_place(&mut self, place: start::Place) {
        use start::Place;
        let trash = self.home.join(".local/share/Trash/files");
        match place {
            Place::Documents => self.files(self.home.clone()),
            Place::MyComputer => self.files("::my-computer"),
            Place::ControlPanel => self.files("::control-panel"),
            Place::RecycleBin => match std::fs::create_dir_all(&trash) {
                Ok(()) => self.files(trash),
                Err(error) => self.message = error.to_string(),
            },
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
            icons::Target::Path(path) => {
                let executable =
                    std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
                if executable {
                    self.launch(Command::new(path));
                } else if let Some(command) =
                    desktop_entry::open_with(Path::new("/usr/share/applications"), &path)
                {
                    self.launch(command);
                } else {
                    self.message = format!("没有与 {} 关联的程序。", icon.label);
                }
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
            Action::DisplayProperties => self.control_panel("display"),
            Action::VolumeProperties => self.launch(Command::new("/bin/sndvol")),
            Action::OpenIcon(index) => self.open_icon(index),
            Action::NewFolder => {
                if let Err(error) = self.icons.new_folder() {
                    self.message = error.to_string();
                }
            }
            Action::ArrangeIcons => {
                if let Err(error) = self.icons.auto_arrange() {
                    self.message = error.to_string();
                }
            }
            Action::Refresh => {
                self.apps = installed_programs();
                self.icons.refresh();
            }
            Action::Window(id, request) => display.control_toplevel(id, request)?,
        }
        Ok(())
    }

    /// 弹出层表面上的事件：菜单点击项执行、点击外部关闭；音量面板拖动滑块、点击静音。
    fn overlay_event(&mut self, display: &mut Display, event: Event) -> io::Result<()> {
        use popups::Overlay;
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
                        let action = menu.hit(x, y).map(|i| menu.items[i].action);
                        self.close_overlay(display)?;
                        if let Some(action) = action {
                            self.perform(display, action)?;
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
                } else if matches!(self.overlay, Some((_, Overlay::Volume(_)))) {
                    self.volume.key(key.code);
                }
            }
            EventKind::Close | EventKind::Focus(false) => self.close_overlay(display)?,
            _ => {}
        }
        Ok(())
    }

    /// 右键：在任务栏/桌面的命中区域上弹出菜单。`local` 是指针在该表面内的位置。
    fn context_menu(&mut self, display: &mut Display, window: WindowId) -> io::Result<()> {
        let Some(c) = display.configuration(window) else {
            return Ok(());
        };
        let (_, x, y) = self.pointer;
        let hit = paint::areas(self, display, window, c).iter().position(|r| {
            x >= f64::from(r.x)
                && x < f64::from(r.right())
                && y >= f64::from(r.y)
                && y < f64::from(r.bottom())
        });
        let (screen, origin) = if window == self.taskbar {
            let screen = Self::screen(display, self.background);
            (screen, (x as i32, screen.1 - TASKBAR_HEIGHT + y as i32))
        } else {
            ((c.width, c.height), (x as i32, y as i32))
        };
        let items = if window == self.taskbar {
            match hit {
                Some(paint::VOLUME) => popups::tray_volume_items(),
                Some(paint::CLOCK) => return Ok(()),
                Some(n) if n >= paint::TASKS_FROM => {
                    let ids = paint::task_ids(self, display, c.width);
                    match ids
                        .get(n - paint::TASKS_FROM)
                        .and_then(|id| display.toplevels().find(|(t, _)| t == id))
                    {
                        Some((id, info)) => popups::window_items(id, info),
                        None => return Ok(()),
                    }
                }
                Some(_) => return Ok(()),
                None => popups::taskbar_items(),
            }
        } else if window == self.background {
            match hit {
                Some(index) => {
                    self.selected = Some(index);
                    popups::icon_items(index)
                }
                None => popups::desktop_items(),
            }
        } else {
            return Ok(());
        };
        let menu = popups::ContextMenu::new(origin, items, screen);
        self.open_overlay(display, popups::Overlay::Menu(menu))
    }

    fn open_volume_panel(&mut self, display: &mut Display, icon_center_x: i32) -> io::Result<()> {
        self.volume = sndvol::Volume::open();
        let screen = Self::screen(display, self.background);
        let panel = popups::volume_panel(icon_center_x, screen, TASKBAR_HEIGHT);
        self.open_overlay(display, popups::Overlay::Volume(panel))
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
    fn event(&mut self, display: &mut Display, event: Event) -> io::Result<()> {
        let window = event.window;
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
        match event.kind {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (window, x, y);
                if let Some(drag) = &mut self.drag
                    && window == self.background
                {
                    drag.pos = (x as i32, y as i32);
                    let start = self.icons.rect(drag.index);
                    drag.moved |= (drag.pos.0 - drag.grab.0 - start.x).abs() > 4
                        || (drag.pos.1 - drag.grab.1 - start.y).abs() > 4;
                }
                if Some(window) == self.menu
                    && let Some(c) = display.configuration(window)
                {
                    let (_, geometry) = self.menu_geometry(c);
                    if let Some(selection) = start::hit(&geometry, (x as i32, y as i32)) {
                        self.menu_selection = selection;
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
                let hit = paint::areas(self, display, window, c).iter().position(|r| {
                    self.pointer.1 >= f64::from(r.x)
                        && self.pointer.1 < f64::from(r.right())
                        && self.pointer.2 >= f64::from(r.y)
                        && self.pointer.2 < f64::from(r.bottom())
                });
                if !pressed && hit.is_none() && Some(window) == self.menu {
                    self.toggle_menu(display)?;
                }
                if pressed {
                    self.armed = hit.map(|i| (window, i));
                    if window == self.background {
                        self.selected = hit;
                        self.drag = hit.map(|index| {
                            let rect = self.icons.rect(index);
                            let pos = (self.pointer.1 as i32, self.pointer.2 as i32);
                            Drag {
                                index,
                                grab: (pos.0 - rect.x, pos.1 - rect.y),
                                pos,
                                moved: false,
                            }
                        });
                    }
                } else if let Some(drag) = self.drag.take().filter(|d| d.moved) {
                    // 拖动结束：落在最近的空格并保存位置，不当作点击。
                    self.armed = None;
                    let screen = Self::screen(display, self.background);
                    let center = (drag.pos.0 - drag.grab.0 + 37, drag.pos.1 - drag.grab.1 + 36);
                    if let Err(error) = self.icons.drop_at(drag.index, center, screen) {
                        self.message = error.to_string();
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
                                if self.clicked.is_some_and(|(previous, time)| {
                                    previous == paint::CLOCK
                                        && time.elapsed() < Duration::from_millis(500)
                                }) {
                                    let mut command = Command::new("/bin/control-panel");
                                    command.arg("datetime");
                                    self.launch(command);
                                    self.clicked = None;
                                } else {
                                    self.clicked = Some((paint::CLOCK, Instant::now()));
                                }
                            }
                            paint::VOLUME => {
                                let center = c.width - 76;
                                self.open_volume_panel(display, center)?
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
                        if self.clicked.is_some_and(|(previous, time)| {
                            previous == index && time.elapsed() < Duration::from_millis(500)
                        }) {
                            self.open_icon(index);
                            self.clicked = None;
                        } else {
                            self.clicked = Some((index, Instant::now()));
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
                        28 if window == self.background => {
                            self.open_icon(self.selected.unwrap_or(0));
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
            } if Some(window) != self.menu => self.context_menu(display, window)?,
            EventKind::Close if Some(window) == self.menu => self.toggle_menu(display)?,
            _ => {}
        }
        Ok(())
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let mut state = Desktop::create(&mut display)?;
    let mut art = paint::Art::load()?;
    let mut dirty = true;
    let mut ready = false;
    let mut presented = std::collections::HashSet::new();
    let mut minute = 0;
    let mut revision = 0;
    let mut pending = std::collections::HashSet::new();
    loop {
        if let Some(event) = display.next_event_timeout(Duration::from_millis(100))? {
            if matches!(event.kind, EventKind::FramePresented { .. }) {
                presented.insert(event.window);
            }
            if !matches!(event.kind, EventKind::FramePresented { .. }) {
                dirty = true;
            }
            state.event(&mut display, event)?;
        }
        dirty |= state.volume.poll();
        dirty |= state.poll_settings(&mut display)?;
        state.tick_auto_hide(&mut display)?;
        state.sync_tooltip(&mut display, &mut art)?;
        if dirty {
            state.sync_task_rectangles(&mut display)?;
        }
        dirty |= state.icons.poll();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
            / 60;
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
            let pixels = paint::frame(&state, &display, id, c, &mut art, minute)?;
            display.present(id, &pixels)?;
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
