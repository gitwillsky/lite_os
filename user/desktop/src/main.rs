//! 用户身份运行的桌面外壳；仅持有 logon 委派的 Shell 连接。
mod dialogs;
mod paint;
mod popups;
mod start;
mod volume;
use classic_ui::{
    Display, Event, EventKind, KeyboardInteractivity, Layer, LayerSettings, ToplevelRequest,
    WindowId, layer_anchor,
};
use dialogs::{Dialog, Kind};
use std::{
    io::{self, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
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
    clicked: Option<(usize, Instant)>,
    task_page: usize,
    home: PathBuf,
    message: String,
    volume: volume::Volume,
}
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
        let taskbar = display.create_layer(
            "org.liteos.taskbar",
            LayerSettings {
                layer: Layer::Top,
                height: TASKBAR_HEIGHT as u32,
                anchor: layer_anchor::LEFT | layer_anchor::RIGHT | layer_anchor::BOTTOM,
                exclusive_zone: TASKBAR_HEIGHT,
                keyboard: KeyboardInteractivity::OnDemand,
                ..Default::default()
            },
        )?;
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
            clicked: None,
            task_page: 0,
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| io::Error::other("HOME missing"))?,
            message: String::new(),
            volume: volume::Volume::open(),
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
        if kind == Kind::Volume {
            self.volume = volume::Volume::open();
        }
        self.dialogs.push(Dialog::new(display, kind)?);
        Ok(())
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

    fn perform(&mut self, display: &mut Display, action: popups::Action) -> io::Result<()> {
        use popups::Action;
        match action {
            Action::MinimizeAll => {
                let ids: Vec<_> = display.toplevels().map(|(id, _)| id).collect();
                for id in ids {
                    display.control_toplevel(id, ToplevelRequest::Minimize(true))?;
                }
            }
            Action::TaskManager => self.dialog(display, Kind::Tasks)?,
            Action::VolumeProperties => self.dialog(display, Kind::Volume)?,
            Action::Open(place) => self.open_place(place),
            Action::Refresh => self.apps = installed_programs(),
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
                        self.volume.motion(&volume::Layout::popup(*panel), y)
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
                        let inside = volume::Contains::contains(&panel, x, y);
                        if pressed && inside {
                            self.volume
                                .button(&volume::Layout::popup(panel), x, y, true);
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
                    popups::icon_items(match index {
                        0 => start::Place::Documents,
                        1 => start::Place::MyComputer,
                        _ => start::Place::RecycleBin,
                    })
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
        self.volume = volume::Volume::open();
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
            Action::Tasks => self.dialog(display, Kind::Tasks)?,
            Action::About => self.dialog(display, Kind::About)?,
            Action::Run => self.dialog(display, Kind::Run)?,
            Action::Lock => self.control.write_all(&[2])?,
            Action::Logoff => std::process::exit(0),
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
        if self.overlay.as_ref().is_some_and(|(w, _)| *w == window) {
            return self.overlay_event(display, event);
        }
        if let Some(index) = self.dialogs.iter().position(|d| d.window == window) {
            match self.dialogs[index].event(display, event, &mut self.volume)? {
                dialogs::Action::Close => {
                    if self.dialogs[index].kind == Kind::Volume {
                        self.volume.cancel();
                    }
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
                                let center = c.width - 118;
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
                            self.open_place(match index {
                                0 => start::Place::Documents,
                                1 => start::Place::MyComputer,
                                _ => start::Place::RecycleBin,
                            });
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
                        1 if key.modifiers.control => self.toggle_menu(display)?,
                        19 if key.modifiers.super_key => self.dialog(display, Kind::Run)?,
                        28 if window == self.background => {
                            self.open_place(match self.selected {
                                Some(1) => start::Place::MyComputer,
                                Some(2) => start::Place::RecycleBin,
                                _ => start::Place::Documents,
                            });
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
            state.dialog(&mut display, Kind::Tasks)?;
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
