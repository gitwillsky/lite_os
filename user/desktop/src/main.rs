//! 用户身份运行的桌面外壳；仅持有 logon 委派的 Shell 连接。
mod dialogs;
mod paint;
mod programs;
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

struct Desktop {
    background: WindowId,
    taskbar: WindowId,
    menu: Option<WindowId>,
    menu_index: usize,
    apps: Vec<programs::Application>,
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
                height: 28,
                anchor: layer_anchor::LEFT | layer_anchor::RIGHT | layer_anchor::BOTTOM,
                exclusive_zone: 28,
                keyboard: KeyboardInteractivity::OnDemand,
                ..Default::default()
            },
        )?;
        let control = UnixStream::from(std::io::stdout().as_fd().try_clone_to_owned()?);
        Ok(Self {
            background,
            taskbar,
            menu: None,
            menu_index: 0,
            apps: programs::load(),
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
            self.menu_index = 0;
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
    fn files(&mut self, path: PathBuf) {
        let mut command = Command::new("/bin/file-explorer");
        command.arg(path);
        self.launch(command);
    }
    fn menu_action(&mut self, display: &mut Display) -> io::Result<()> {
        let selected = self.menu_index;
        self.toggle_menu(display)?;
        if let Some(app) = self.apps.get(selected) {
            self.launch(app.command());
            return Ok(());
        }
        match selected.saturating_sub(self.apps.len()) {
            0 => self.files(self.home.clone()),
            1 => self.dialog(display, Kind::Tasks)?,
            2 => self.dialog(display, Kind::About)?,
            3 => self.dialog(display, Kind::Run)?,
            4 => self.control.write_all(&[2])?,
            5 => std::process::exit(0),
            6 => self.control.write_all(&[4])?,
            _ => {}
        }
        Ok(())
    }
    fn event(&mut self, display: &mut Display, event: Event) -> io::Result<()> {
        let window = event.window;
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
                    let rect = paint::menu_rect(self, c);
                    if (rect.y..rect.bottom()).contains(&(y as i32)) {
                        self.menu_index = ((y as i32 - rect.y - 4) / 32).max(0) as usize;
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
                            2 => self.files(self.home.clone()),
                            3 => self.task_page = self.task_page.saturating_sub(1),
                            4 => self.task_page += 1,
                            5 => self.dialog(display, Kind::Volume)?,
                            n => {
                                let ids = paint::task_ids(self, display, c.width);
                                if let Some(id) = ids.get(n - 6) {
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
                    } else if Some(window) == self.menu {
                        self.menu_index = index;
                        self.menu_action(display)?;
                    } else if window == self.background {
                        self.selected = Some(index);
                        if self.clicked.is_some_and(|(previous, time)| {
                            previous == index && time.elapsed() < Duration::from_millis(500)
                        }) {
                            let path = match index {
                                0 => self.home.clone(),
                                1 => PathBuf::from("/"),
                                _ => self.home.join(".local/share/Trash/files"),
                            };
                            if index == 2
                                && let Err(error) = std::fs::create_dir_all(&path)
                            {
                                self.message = error.to_string();
                                return Ok(());
                            }
                            self.files(path);
                            self.clicked = None;
                        } else {
                            self.clicked = Some((index, Instant::now()));
                        }
                    }
                }
            }
            EventKind::Key(key) if key.pressed => {
                if Some(window) == self.menu {
                    let count = self.apps.len() + 7;
                    match key.code {
                        1 | 125 | 126 => self.toggle_menu(display)?,
                        103 => self.menu_index = (self.menu_index + count - 1) % count,
                        108 => self.menu_index = (self.menu_index + 1) % count,
                        28 => self.menu_action(display)?,
                        _ => {}
                    }
                } else {
                    match key.code {
                        125 | 126 => self.toggle_menu(display)?,
                        1 if key.modifiers.control => self.toggle_menu(display)?,
                        19 if key.modifiers.super_key => self.dialog(display, Kind::Run)?,
                        28 if window == self.background => {
                            let path = if self.selected == Some(1) {
                                PathBuf::from("/")
                            } else {
                                self.home.clone()
                            };
                            self.files(path);
                        }
                        62 if key.modifiers.alt && window == self.background => {
                            self.control.write_all(&[4])?
                        }
                        _ => {}
                    }
                }
            }
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
