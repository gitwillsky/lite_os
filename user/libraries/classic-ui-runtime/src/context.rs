//! UI context 独占 Display、窗口、字体资源和等待；不调用应用代码，不持跨 ABI 借用。
use super::widgets::{Tree, invalid};
use classic_ui::{Display, EventKind, FrameKind, Resources, WindowHost};
use classic_ui_abi::{self as abi, event};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::Path,
    time::Duration,
};

pub enum WindowBody {
    Widgets(Tree),
    Sheet(super::property_sheet::Sheet),
    Dialog(super::file_dialog::FileDialog),
}
pub struct WindowRecord {
    pub title: String,
    pub app_id: String,
    pub parent: Option<u64>,
    pub modal: bool,
    pub frame: WindowHost,
    pub body: WindowBody,
}
impl WindowRecord {
    pub fn controls(&self) -> io::Result<&Tree> {
        match &self.body {
            WindowBody::Widgets(tree) => Ok(tree),
            WindowBody::Sheet(sheet) => Ok(&sheet.tree),
            WindowBody::Dialog(dialog) => Ok(&dialog.tree),
        }
    }
    pub fn controls_mut(&mut self) -> io::Result<&mut Tree> {
        match &mut self.body {
            WindowBody::Widgets(tree) => {
                tree.layout_dirty = true;
                Ok(tree)
            }
            WindowBody::Sheet(sheet) => {
                sheet.tree.layout_dirty = true;
                Ok(&mut sheet.tree)
            }
            WindowBody::Dialog(dialog) => {
                dialog.tree.layout_dirty = true;
                Ok(&mut dialog.tree)
            }
        }
    }
}
pub struct Watch {
    pub fd: OwnedFd,
    pub interest: u32,
}
pub struct Context {
    pub display: Display,
    pub windows: BTreeMap<u64, WindowRecord>,
    next: u64,
    pub error: String,
    pub(super) events: VecDeque<abi::Event>,
    resources: Resources,
    files: Option<super::file_dialog::FileWorker>,
    // 本轮 FD/结果容量由 context 复用，PollSet 的有效借用结束后才改变 watch 集合。
    poll_buffer: linux_uapi::unix::PollBuffer,
    ready: Vec<(u64, u32)>,
    pub watches: BTreeMap<u64, Watch>,
    pub next_watch: u64,
    pub pastes: BTreeMap<u64, super::clipboard::Paste>,
    pub foreign: BTreeMap<u64, classic_ui::ToplevelId>,
    pub next_foreign: u64,
}
impl Context {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            display: Display::connect()?,
            windows: BTreeMap::new(),
            next: 1,
            error: String::new(),
            events: VecDeque::new(),
            resources: Resources::system()?,
            files: None,
            poll_buffer: Default::default(),
            ready: Vec::new(),
            watches: BTreeMap::new(),
            next_watch: 1,
            pastes: BTreeMap::new(),
            foreign: BTreeMap::new(),
            next_foreign: 1,
        })
    }
    pub fn window(
        &mut self,
        title: &str,
        app_id: &str,
        width: i32,
        height: i32,
        fixed: bool,
        parent: u64,
        modal: bool,
    ) -> io::Result<u64> {
        if self.windows.len() >= 256 {
            return Err(invalid("window limit reached"));
        }
        if modal && parent == 0 {
            return Err(invalid("modal window requires a parent"));
        }
        let parent_id = (parent != 0).then_some(parent);
        let parent = if parent == 0 {
            None
        } else {
            Some(self.host(parent)?.frame.window())
        };
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("window identities exhausted"))?;
        let protocol = self.display.create_window(title, app_id, (width, height))?;
        let setup = (|| {
            if let Some(parent) = parent {
                self.display.set_parent(protocol, parent)?;
            }
            if fixed {
                self.display.fix_size(protocol)?;
            }
            if modal {
                self.display.set_modal(protocol, true)?;
            }
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = setup {
            let _ = self.display.destroy_window(protocol);
            return Err(error);
        }
        let mut frame = WindowHost::new(
            protocol,
            if fixed {
                FrameKind::Fixed
            } else {
                FrameKind::Resizable
            },
        );
        frame.set_icon(self.resources.application_icon(app_id));
        self.windows.insert(
            id,
            WindowRecord {
                title: title.into(),
                app_id: app_id.into(),
                parent: parent_id,
                modal,
                frame,
                body: WindowBody::Widgets(Tree::new()),
            },
        );
        Ok(id)
    }
    pub fn file_dialog(
        &mut self,
        parent: u64,
        mode: u32,
        directory: &Path,
        name: &str,
        filter: Option<super::file_dialog::Filter>,
    ) -> io::Result<u64> {
        use super::file_dialog::{FileDialog, FileWorker};
        let title = match mode {
            0 => "打开",
            1 => "另存为",
            _ => return Err(invalid("unknown file dialog mode")),
        };
        if self.files.is_none() {
            let watch = self.next_watch;
            self.next_watch = watch
                .checked_add(1)
                .ok_or_else(|| invalid("FD identities exhausted"))?;
            let worker = FileWorker::new(watch)?;
            let fd = worker.reader.as_fd().try_clone_to_owned()?;
            self.watches.insert(watch, Watch { fd, interest: 1 });
            self.files = Some(worker);
        }
        let app_id = self.host(parent)?.app_id.clone();
        let mut model = FileDialog::new(mode, directory.to_owned(), name, filter)?;
        let id = self.window(title, &app_id, 480, 370, false, parent, true)?;
        let action = model.initial(id);
        self.host_mut(id)?.body = WindowBody::Dialog(model);
        self.file_action(id, action);
        Ok(id)
    }
    pub(super) fn file_action(&mut self, id: u64, action: super::file_dialog::Action) {
        use super::file_dialog::Action;
        match action {
            Action::Pending => {}
            Action::Done => {
                let accepted = self.windows.get(&id).is_some_and(|host| matches!(&host.body, WindowBody::Dialog(dialog) if matches!(dialog.outcome, Some(Some(_)))));
                self.events.push_back(abi::Event {
                    window: id,
                    kind: event::DIALOG_DONE,
                    flags: u32::from(accepted),
                    ..Default::default()
                });
            }
            Action::Request(request) => {
                let result = self.files.as_ref().expect("file worker").submit(request);
                if let Err(error) = result {
                    if let Some(host) = self.windows.get_mut(&id) {
                        if let WindowBody::Dialog(dialog) = &mut host.body {
                            dialog.failed(error);
                        }
                        host.frame.invalidate();
                    }
                }
            }
        }
    }
    fn progress_files(&mut self) -> io::Result<()> {
        let Some(worker) = &mut self.files else {
            return Ok(());
        };
        let completions = worker.drain()?;
        if let Some(watch) = self.watches.get_mut(&worker.watch) {
            watch.interest = 1;
        }
        for completion in completions {
            let id = completion.window;
            let Some(host) = self.windows.get_mut(&id) else {
                continue;
            };
            let WindowBody::Dialog(dialog) = &mut host.body else {
                continue;
            };
            let action = dialog.apply(completion);
            host.frame.invalidate();
            self.file_action(id, action);
        }
        Ok(())
    }
    fn blocked(&self, target: u64) -> bool {
        self.windows.iter().any(|(id, child)| {
            if *id == target || !child.modal {
                return false;
            }
            let mut ancestor = child.parent;
            while let Some(id) = ancestor {
                if id == target {
                    return true;
                }
                ancestor = self.windows.get(&id).and_then(|window| window.parent);
            }
            false
        })
    }
    pub fn host(&self, id: u64) -> io::Result<&WindowRecord> {
        self.windows
            .get(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown window"))
    }
    pub fn host_mut(&mut self, id: u64) -> io::Result<&mut WindowRecord> {
        self.windows
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown window"))
    }
    pub fn close(&mut self, id: u64) -> io::Result<()> {
        let children: Vec<_> = self
            .windows
            .iter()
            .filter_map(|(child, host)| (host.parent == Some(id)).then_some(*child))
            .collect();
        for child in children {
            self.close(child)?;
        }
        self.cancel_pastes(|paste| paste.window == id);
        let host = self
            .windows
            .remove(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown window"))?;
        self.events.retain(|event| event.window != id);
        self.display.destroy_window(host.frame.window())
    }
    pub fn step(
        &mut self,
        timeout: Option<Duration>,
        wake: Option<BorrowedFd<'_>>,
    ) -> io::Result<Option<abi::Event>> {
        self.paint()?;
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        let timeout = timeout
            .into_iter()
            .chain(self.pastes.values().map(|paste| {
                paste
                    .deadline
                    .saturating_duration_since(std::time::Instant::now())
            }))
            .min();
        self.ready.clear();
        let incoming = {
            use linux_uapi::unix::{PollEvents, PollFd};
            let mut sources = self.poll_buffer.prepare();
            if let Some(fd) = wake {
                sources.push(PollFd::new(fd, PollEvents::READ));
            }
            for watch in self.watches.values().filter(|watch| watch.interest != 0) {
                let interest = match watch.interest {
                    1 => PollEvents::READ,
                    2 => PollEvents::WRITE,
                    _ => PollEvents::READ | PollEvents::WRITE,
                };
                sources.push(PollFd::new(watch.fd.as_fd(), interest));
            }
            let incoming = self
                .display
                .next_event_with_sources(timeout, sources.sources())?;
            for ((id, _), source) in self
                .watches
                .iter()
                .filter(|(_, watch)| watch.interest != 0)
                .zip(sources.sources().iter().skip(usize::from(wake.is_some())))
            {
                let flags = source.returned();
                let mask = u32::from(flags.contains(PollEvents::READ))
                    | (u32::from(flags.contains(PollEvents::WRITE)) << 1)
                    | (u32::from(flags.contains(PollEvents::ERROR)) << 2)
                    | (u32::from(flags.contains(PollEvents::HANGUP)) << 3);
                if mask != 0 {
                    self.ready.push((*id, mask));
                }
            }
            incoming
        };
        // 粘贴并发上限为 8；固定栈存储收集超时编号，释放迭代借用后再撤销。
        let mut expired = [0; 8];
        let mut expired_count = 0;
        for (id, paste) in &self.pastes {
            if paste.deadline <= std::time::Instant::now() {
                expired[expired_count] = *id;
                expired_count += 1;
            }
        }
        for id in &expired[..expired_count] {
            self.progress_paste(*id);
        }
        for index in 0..self.ready.len() {
            let (id, flags) = self.ready[index];
            if self.files.as_ref().is_some_and(|worker| worker.watch == id) {
                self.progress_files()?;
                continue;
            }
            if self.progress_paste(id) {
                continue;
            }
            if let Some(watch) = self.watches.get_mut(&id) {
                watch.interest = 0;
                self.events.push_back(abi::Event {
                    kind: event::FD_READY,
                    value: id,
                    flags,
                    ..Default::default()
                });
            }
        }
        if let Some(input) = incoming {
            if matches!(input.kind, EventKind::ToplevelsChanged) {
                self.events.push_back(abi::Event {
                    kind: event::TOPLEVELS_CHANGED,
                    ..Default::default()
                });
                return Ok(self.events.pop_front());
            }
            let logical_window = self
                .windows
                .iter()
                .find_map(|(id, host)| (host.frame.window() == input.window).then_some(*id));
            if logical_window.is_some_and(|window| self.blocked(window))
                && matches!(
                    input.kind,
                    EventKind::Key(_)
                        | EventKind::Button { .. }
                        | EventKind::Wheel { .. }
                        | EventKind::PointerEnter { .. }
                        | EventKind::PointerMove { .. }
                        | EventKind::Close
                )
            {
                // xdg-dialog 明确要求客户端筛选；仍处理 configure、释放和失焦，不能阻塞协议推进。
                self.paint()?;
                return Ok(self.events.pop_front());
            }
            let clipboard_handled =
                if let (Some(window), EventKind::Key(key)) = (logical_window, &input.kind) {
                    if self.host(window)?.controls().is_ok() {
                        self.edit_clipboard(window, *key)?
                    } else {
                        false
                    }
                } else {
                    false
                };

            let Some((&id, host)) = self
                .windows
                .iter_mut()
                .find(|(_, host)| host.frame.window() == input.window)
            else {
                return Ok(None);
            };
            let Some(_configuration) = self.display.configuration(input.window) else {
                return Ok(None);
            };
            let mut handled = clipboard_handled;
            let close = host.frame.event(&mut self.display, &input)?.close;
            let mut file_action = super::file_dialog::Action::Pending;
            match &mut host.body {
                WindowBody::Widgets(tree) => {
                    if close {
                        self.events.push_back(abi::Event {
                            window: id,
                            kind: event::CLOSE,
                            ..Default::default()
                        });
                    }
                    let configuration = self
                        .display
                        .configuration(input.window)
                        .expect("configured input");
                    let font = self.resources.font(configuration.scale)?;
                    let mut scratch = [0; 1];
                    let surface =
                        canvas::Surface::new(&mut scratch, 1, 1, 1).expect("one pixel surface");
                    let probe = classic_ui::Painter::new(
                        surface,
                        self.resources.theme(),
                        &font,
                        configuration.scale,
                    );
                    let (actions, consumed, repaint) =
                        tree.input(id, &input.kind, &probe, self.display.keyboard_modifiers());
                    handled |= consumed;
                    if repaint {
                        host.frame.invalidate();
                    }
                    self.events.extend(actions);
                    if host.frame.client_rect(&self.display).is_some_and(|r| {
                        (r.x..r.right()).contains(&tree.pointer.0)
                            && (r.y..r.bottom()).contains(&tree.pointer.1)
                    }) {
                        self.display.set_cursor_shape(tree.cursor())?;
                    }
                }
                WindowBody::Sheet(sheet) => {
                    let configuration = self
                        .display
                        .configuration(input.window)
                        .expect("configured sheet");
                    let font = self.resources.font(configuration.scale)?;
                    let mut scratch = [0; 1];
                    let surface =
                        canvas::Surface::new(&mut scratch, 1, 1, 1).expect("one pixel surface");
                    let probe = classic_ui::Painter::new(
                        surface,
                        self.resources.theme(),
                        &font,
                        configuration.scale,
                    );
                    let (actions, consumed, repaint) = sheet.tree.input(
                        id,
                        &input.kind,
                        &probe,
                        self.display.keyboard_modifiers(),
                    );
                    handled |= consumed;
                    if repaint {
                        host.frame.invalidate();
                    }
                    if host.frame.client_rect(&self.display).is_some_and(|r| {
                        (r.x..r.right()).contains(&sheet.tree.pointer.0)
                            && (r.y..r.bottom()).contains(&sheet.tree.pointer.1)
                    }) {
                        self.display.set_cursor_shape(sheet.tree.cursor())?;
                    }
                    let escape =
                        matches!(&input.kind,EventKind::Key(key) if key.pressed&&key.code==1);
                    self.events
                        .extend(sheet.actions(id, actions, close || escape)?);
                }
                WindowBody::Dialog(dialog) => {
                    let configuration = self
                        .display
                        .configuration(input.window)
                        .expect("configured dialog");
                    let font = self.resources.font(configuration.scale)?;
                    let mut scratch = [0; 1];
                    let probe = classic_ui::Painter::new(
                        canvas::Surface::new(&mut scratch, 1, 1, 1).expect("probe surface"),
                        self.resources.theme(),
                        &font,
                        configuration.scale,
                    );
                    let (actions, consumed, repaint) = dialog.tree.input(
                        id,
                        &input.kind,
                        &probe,
                        self.display.keyboard_modifiers(),
                    );
                    handled |= consumed;
                    if repaint {
                        host.frame.invalidate();
                    }
                    file_action = dialog.actions(id, actions, &input.kind, close);
                    if host.frame.client_rect(&self.display).is_some_and(|r| {
                        (r.x..r.right()).contains(&dialog.tree.pointer.0)
                            && (r.y..r.bottom()).contains(&dialog.tree.pointer.1)
                    }) {
                        self.display.set_cursor_shape(dialog.tree.cursor())?;
                    }
                }
            }

            let mut event = abi::Event {
                window: id,
                ..Default::default()
            };
            match input.kind {
                EventKind::Close => {}
                EventKind::Configure(c) => {
                    event.kind = event::CONFIGURE;
                    event.width = c.width;
                    event.height = c.height;
                    event.scale = c.scale;
                }
                EventKind::Focus(active) => {
                    event.kind = event::FOCUS;
                    event.flags = u32::from(active);
                }
                EventKind::Key(key) => {
                    event.kind = event::KEY;
                    event.code = key.code;
                    event.flags = u32::from(key.pressed)
                        | (u32::from(key.repeat) << 1)
                        | (u32::from(handled) << 2);
                    event.modifiers = u32::from(key.modifiers.shift)
                        | (u32::from(key.modifiers.control) << 1)
                        | (u32::from(key.modifiers.alt) << 2)
                        | (u32::from(key.modifiers.super_key) << 3);
                    event.text = key.text.map_or(0, |v| v as u32);
                }
                EventKind::Button {
                    code,
                    pressed,
                    serial,
                } => {
                    event.kind = event::BUTTON;
                    event.code = code;
                    event.flags = u32::from(pressed);
                    event.value = u64::from(serial);
                }
                EventKind::Wheel {
                    vertical,
                    horizontal,
                } => {
                    event.kind = event::WHEEL;
                    event.width = vertical;
                    event.height = horizontal;
                }
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    event.kind = event::POINTER;
                    event.x = x;
                    event.y = y;
                }
                _ => {}
            }
            if let Ok(tree) = host.controls() {
                event.node = if matches!(event.kind, event::KEY) {
                    tree.focus.unwrap_or(0)
                } else {
                    tree.hit().unwrap_or(0)
                };
            }
            if event.kind != 0 {
                self.events.push_back(event);
            }
            self.file_action(id, file_action);
            self.paint()?;
        }
        Ok(self.events.pop_front())
    }
    fn paint(&mut self) -> io::Result<()> {
        let needs_mono = self.windows.values().any(|host| {
            host.controls().is_ok_and(|tree| {
                tree.nodes
                    .values()
                    .any(|node| node.drawing.iter().any(|command| command.kind == 5))
            })
        });
        let mono = if needs_mono {
            Some(self.resources.terminal_font()?)
        } else {
            None
        };
        for host in self.windows.values_mut() {
            let protocol = host.frame.window();
            layout_record(host, &self.display, &mut self.resources)?;
            let body = &mut host.body;
            host.frame.paint(
                &mut self.display,
                &mut self.resources,
                &host.title,
                |painter, _client, _, display| {
                    match body {
                        WindowBody::Widgets(tree) => {
                            tree.paint(
                                painter,
                                display.keyboard_focus() == Some(protocol),
                                mono.as_deref(),
                            );
                        }
                        WindowBody::Sheet(sheet) => {
                            sheet.tree.paint(
                                painter,
                                display.keyboard_focus() == Some(protocol),
                                mono.as_deref(),
                            );
                        }
                        WindowBody::Dialog(dialog) => dialog.tree.paint(
                            painter,
                            display.keyboard_focus() == Some(protocol),
                            mono.as_deref(),
                        ),
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    }
}

impl Context {
    /// 仅运行库编辑入口调用；单行/多行控件共用结果通知，业务层不复制光标或选区。
    pub(super) fn change_edit(
        &mut self,
        window: u64,
        id: u64,
        command: u32,
        value: &str,
    ) -> io::Result<()> {
        let protocol = self.host(window)?.frame.window();
        let scale = self.display.configuration(protocol).map_or(1, |c| c.scale);
        let font = self.resources.font(scale)?;
        let mut scratch = [0; 1];
        let probe = classic_ui::Painter::new(
            canvas::Surface::new(&mut scratch, 1, 1, 1).expect("probe surface"),
            self.resources.theme(),
            &font,
            scale,
        );
        let host = self
            .windows
            .get_mut(&window)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown window"))?;
        let tree = host.controls_mut()?;
        if !tree.active(id) {
            return Err(invalid("editor is unavailable"));
        }
        let node = tree
            .nodes
            .get_mut(&id)
            .ok_or_else(|| invalid("unknown edit control"))?;
        if !node.editing_text() {
            return Err(invalid("editor is unavailable"));
        }
        let changed = if let Some(editor) = &mut node.editor {
            if command == 4 {
                editor.insert(node.rect, &probe, value)
            } else {
                editor.command(node.rect, &probe, command)
            }
        } else if let Some(edit) = &mut node.edit {
            let before = edit.text().to_owned();
            match command {
                1 | 4 => {
                    edit.insert_text(if command == 1 { "" } else { value });
                }
                2 | 3 => {
                    let mut key = classic_ui::Key {
                        code: if command == 2 { 111 } else { 30 },
                        pressed: true,
                        repeat: false,
                        modifiers: Default::default(),
                        text: None,
                    };
                    key.modifiers.control = command == 3;
                    edit.key(key);
                }
                _ => return Err(invalid("unknown edit action")),
            }
            before != edit.text()
        } else {
            return Err(invalid("not an edit control"));
        };
        host.frame.invalidate();
        if changed {
            self.publish(Some(abi::Event {
                window,
                node: id,
                kind: event::CHANGED,
                ..Default::default()
            }));
        }
        Ok(())
    }
}

impl Context {
    pub(super) fn icon_asset(
        &mut self,
        name: &str,
        kind: u32,
        size: i32,
    ) -> Option<std::rc::Rc<classic_ui::Bitmap>> {
        if kind == 0 {
            self.resources.icon(name, size)
        } else {
            self.resources.application_icon_size(name, size)
        }
    }
}

fn layout_record(
    host: &mut WindowRecord,
    display: &Display,
    resources: &mut Resources,
) -> io::Result<()> {
    let protocol = host.frame.window();
    if let Some(configuration) = display.configuration(protocol) {
        if let Some(bounds) = host.frame.client_rect(&display) {
            let tree = match &mut host.body {
                WindowBody::Widgets(tree) => Some(tree),
                WindowBody::Sheet(sheet) => Some(&mut sheet.tree),
                WindowBody::Dialog(dialog) => Some(&mut dialog.tree),
            };
            if let Some(tree) = tree.filter(|tree| {
                tree.layout_dirty || tree.layout_bounds != Some((bounds, configuration.scale))
            }) {
                let font = resources.font(configuration.scale)?;
                let mut scratch = [0; 1];
                let surface = canvas::Surface::new(&mut scratch, 1, 1, 1).expect("probe surface");
                let probe = classic_ui::Painter::new(
                    surface,
                    resources.theme(),
                    &font,
                    configuration.scale,
                );
                tree.layout(bounds, &probe);
            }
        }
    }
    Ok(())
}
impl Context {
    pub(super) fn layout(&mut self, window: u64) -> io::Result<()> {
        let host = self
            .windows
            .get_mut(&window)
            .ok_or_else(|| invalid("unknown window"))?;
        layout_record(host, &self.display, &mut self.resources)
    }
    pub(super) fn color(&self, role: u32) -> io::Result<u32> {
        use classic_theme::SystemColor as C;
        let color = match role {
            0 => C::WindowText,
            1 => C::Window,
            2 => C::ButtonFace,
            3 => C::Background,
            4 => C::Hilight,
            5 => C::HilightText,
            6 => C::GrayText,
            _ => return Err(invalid("unknown color role")),
        };
        let color = self.resources.theme().color(color);
        Ok(u32::from(color.red) << 16 | u32::from(color.green) << 8 | u32::from(color.blue))
    }
    pub(super) fn app_id(&mut self, window: u64, value: &str) -> io::Result<()> {
        let host = self.host(window)?;
        if host.app_id == value {
            return Ok(());
        }
        self.display.set_app_id(host.frame.window(), value)?;
        let icon = self.resources.application_icon(value);
        let host = self.host_mut(window)?;
        host.app_id = value.into();
        host.frame.set_icon(icon);
        Ok(())
    }
}

impl Context {
    pub(super) fn popup(&mut self, window: u64, id: u64) -> io::Result<()> {
        self.layout(window)?;
        let protocol = self.host(window)?.frame.window();
        let configuration = self
            .display
            .configuration(protocol)
            .ok_or_else(|| invalid("window is not configured"))?;
        let font = self.resources.font(configuration.scale)?;
        let mut scratch = [0; 1];
        let probe = classic_ui::Painter::new(
            canvas::Surface::new(&mut scratch, 1, 1, 1).expect("probe surface"),
            self.resources.theme(),
            &font,
            configuration.scale,
        );
        let host = self.windows.get_mut(&window).expect("validated window");
        let bounds = host
            .frame
            .client_rect(&self.display)
            .expect("configured window");
        let tree = host.controls_mut()?;
        let point = tree.pointer;
        let menu = tree
            .nodes
            .get_mut(&id)
            .filter(|node| node.kind == abi::widget::POPUP_MENU)
            .and_then(|node| node.menu.as_mut())
            .ok_or_else(|| invalid("not a popup menu"))?;
        menu.state.show_popup(&probe, &menu.entries, point, bounds);
        host.frame.invalidate();
        Ok(())
    }
}
