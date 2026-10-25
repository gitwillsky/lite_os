//! 标准 Wayland 客户端窗口。连接独占协议对象、configure、输入焦点与帧生命周期。

mod events;
mod foreign;
mod frame;
pub use foreign::{Toplevel, ToplevelId, ToplevelRequest};
mod layer;
mod security;
mod transport;

use keyboard::Modifiers;
pub use layer::{KeyboardInteractivity, Layer, LayerSettings, layer_anchor};
use linux_uapi::{
    shared_memory::MemFd,
    unix::{PollEvents, PollFd, poll},
};
pub use security::{ShellConnection, ShellLease};
use std::{
    collections::{HashMap, VecDeque},
    io,
    os::fd::AsFd,
    time::{Duration, Instant},
};
use transport::Transport;
pub use wayland_wire::protocol::xdg_toplevel::ResizeEdge;
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_compositor, wl_display, wl_registry, wl_surface, xdg_surface,
        xdg_toplevel, xdg_wm_base,
    },
    wire::{DynamicNewId, ObjectId},
};

/// 本连接中的窗口标识；只能交给创建它的 [`Display`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(ObjectId);

/// 一次已确认的窗口配置，宽高为整个 surface 的逻辑像素（包括客户端装饰）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub active: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub resizing: bool,
}

impl Configuration {
    /// 返回物理缓冲尺寸；非法或无法表达的大小返回 `InvalidInput`。
    pub fn pixel_size(self) -> io::Result<(i32, i32)> {
        let (width, height) = (
            self.width.checked_mul(self.scale),
            self.height.checked_mul(self.scale),
        );
        match (width, height) {
            (Some(width), Some(height)) if width > 0 && height > 0 && self.scale > 0 => {
                Ok((width, height))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Wayland buffer size",
            )),
        }
    }
}

/// 已按服务端 modifiers 解释的按键；`text` 只在可打印且没有 Ctrl/Alt/Super 时提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: u32,
    pub pressed: bool,
    pub repeat: bool,
    pub modifiers: Modifiers,
    pub text: Option<char>,
}

/// 应用看到的窗口事件；坐标为 surface 局部的逻辑像素，保留小数。
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    Configure(Configuration),
    Close,
    PointerEnter {
        x: f64,
        y: f64,
    },
    PointerMove {
        x: f64,
        y: f64,
    },
    PointerLeave,
    Button {
        code: u32,
        pressed: bool,
        serial: u32,
    },
    Key(Key),
    Focus(bool),
    FramePresented {
        time: u32,
    },
}

/// 一个窗口的语义事件。
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub window: WindowId,
    pub kind: EventKind,
}

/// 交给 compositor 的窗口管理请求；移动/缩放序号必须来自按钮按下事件。
#[derive(Debug, Clone, Copy)]
pub enum WindowRequest {
    Move { serial: u32 },
    Resize { serial: u32, edges: ResizeEdge },
    Maximize(bool),
    Fullscreen(bool),
    Minimize,
}

enum Role {
    Toplevel {
        xdg: ObjectId,
        toplevel: ObjectId,
        /// 最后一条 toplevel.configure，到配套 surface.configure 才发布为当前配置。
        pending: Configuration,
    },
    Layer {
        object: ObjectId,
    },
}

struct Window {
    role: Role,
    configuration: Configuration,
    /// 初始 configure 前不能提交；否则会违反 xdg-shell 的首次映射握手。
    configured: bool,
    /// 前一帧 callback 未到前不再提交，防止应用无限积压 shm 缓冲与渲染工作。
    frame: Option<ObjectId>,
}

struct Repeat {
    window: WindowId,
    code: u32,
    next: Instant,
}

/// 单线程的显示连接；drop 关闭 socket 并回收本地 shm/fd，服务端随连接清理全部窗口。
///
/// 当前使用 LiteOS 的固定 US XKB keymap；其他 keymap 明确返回 `Unsupported`。
pub struct Display {
    transport: Transport,
    compositor: ObjectId,
    shm: ObjectId,
    wm_base: ObjectId,
    seat: ObjectId,
    scale: i32,
    windows: HashMap<WindowId, Window>,
    buffers: HashMap<ObjectId, MemFd>,
    events: VecDeque<Event>,
    pointer: Option<WindowId>,
    keyboard: Option<WindowId>,
    modifiers: Modifiers,
    repeat: Option<Repeat>,
    repeat_rate: i32,
    repeat_delay: i32,
    pointer_device: Option<ObjectId>,
    keyboard_device: Option<ObjectId>,
    layer_shell: Option<ObjectId>,
    security_manager: Option<ObjectId>,
    foreign: std::collections::BTreeMap<u32, foreign::Pending>,
    foreign_revision: u64,
}

impl Display {
    /// 按 WAYLAND_SOCKET 或 WAYLAND_DISPLAY/XDG_RUNTIME_DIR 连接，并绑定窗口与输入协议。
    ///
    /// # Errors
    ///
    /// socket、协议握手失败或缺少 compositor/shm/xdg-shell/seat/output。
    pub fn connect() -> io::Result<Self> {
        // 1. 收集 registry 全局对象，sync 划定初始列表的边界。
        let mut transport = Transport::connect()?;
        let registry = transport.allocate(Interface::WlRegistry)?;
        let callback = transport.allocate(Interface::WlCallback)?;
        wl_display::Request::GetRegistry { registry }
            .encode(ObjectId::DISPLAY, &mut transport.writer)
            .map_err(io::Error::other)?;
        wl_display::Request::Sync { callback }
            .encode(ObjectId::DISPLAY, &mut transport.writer)
            .map_err(io::Error::other)?;
        let mut globals = Vec::new();
        loop {
            let (object, event) = transport.blocking_event()?;
            match event {
                AnyEvent::WlRegistry(wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                }) => globals.push((name, interface, version)),
                AnyEvent::WlCallback(_) if object == callback => break,
                _ => {}
            }
        }
        // 2. 按支持版本绑定，拒绝无法设置 buffer scale 的 compositor。
        let mut bind = |interface: Interface, wanted: u32| -> io::Result<ObjectId> {
            let (name, _, version) = globals
                .iter()
                .find(|(_, name, _)| name == interface.name())
                .ok_or_else(|| {
                    io::Error::other(format!("missing Wayland global {}", interface.name()))
                })?;
            if interface == Interface::WlCompositor && *version < 3 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "classic-ui requires wl_compositor v3 for buffer scale",
                ));
            }
            let object = transport.allocate(interface)?;
            wl_registry::Request::Bind {
                name: *name,
                id: DynamicNewId {
                    interface: interface.name().into(),
                    version: wanted.min(*version),
                    id: object,
                },
            }
            .encode(registry, &mut transport.writer)
            .map_err(io::Error::other)?;
            Ok(object)
        };
        let compositor = bind(Interface::WlCompositor, 6)?;
        let shm = bind(Interface::WlShm, 1)?;
        let wm_base = bind(Interface::XdgWmBase, 3)?;
        let seat = bind(Interface::WlSeat, 7)?;
        let _output = bind(Interface::WlOutput, 4)?;
        let layer_shell = if let Some((_, _, version)) = globals
            .iter()
            .find(|(_, name, _)| name == "zwlr_layer_shell_v1")
        {
            if *version < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "classic-ui requires layer-shell v4 for OnDemand keyboard input",
                ));
            }
            Some(bind(Interface::ZwlrLayerShellV1, 5)?)
        } else {
            None
        };
        let security_manager = if globals
            .iter()
            .any(|(_, name, _)| name == "wp_security_context_manager_v1")
        {
            Some(bind(Interface::WpSecurityContextManagerV1, 1)?)
        } else {
            None
        };
        if globals
            .iter()
            .any(|(_, name, _)| name == "zwlr_foreign_toplevel_manager_v1")
        {
            bind(Interface::ZwlrForeignToplevelManagerV1, 3)?;
        }
        let mut display = Self {
            transport,
            compositor,
            shm,
            wm_base,
            seat,
            scale: 1,
            windows: HashMap::new(),
            buffers: HashMap::new(),
            events: VecDeque::new(),
            pointer: None,
            keyboard: None,
            modifiers: Modifiers::default(),
            repeat: None,
            repeat_rate: 0,
            repeat_delay: 0,
            pointer_device: None,
            keyboard_device: None,
            layer_shell,
            security_manager,
            foreign: Default::default(),
            foreign_revision: 0,
        };
        // 3. 两轮 sync 分别收齐 output/seat，以及由 seat 能力请求产生的 keymap，之后才允许建窗口。
        display.roundtrip()?;
        display.roundtrip()?;
        Ok(display)
    }

    fn roundtrip(&mut self) -> io::Result<()> {
        let callback = self.transport.allocate(Interface::WlCallback)?;
        wl_display::Request::Sync { callback }
            .encode(ObjectId::DISPLAY, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        loop {
            let (object, event) = self.transport.blocking_event()?;
            if object == callback {
                break;
            }
            self.dispatch(object, event)?;
        }
        Ok(())
    }

    /// 创建有 xdg-toplevel 角色的窗口；`size` 是首选逻辑尺寸，等待 Configure 后才能绘制。
    ///
    /// # Errors
    ///
    /// 尺寸非正、字符串过长或协议/socket 失败。
    pub fn create_window(
        &mut self,
        title: &str,
        app_id: &str,
        size: (i32, i32),
    ) -> io::Result<WindowId> {
        let configuration = Configuration {
            width: size.0,
            height: size.1,
            scale: self.scale,
            active: false,
            maximized: false,
            fullscreen: false,
            resizing: false,
        };
        configuration.pixel_size()?;
        let surface = self.transport.allocate(Interface::WlSurface)?;
        let xdg = self.transport.allocate(Interface::XdgSurface)?;
        let toplevel = self.transport.allocate(Interface::XdgToplevel)?;
        let writer = &mut self.transport.writer;
        wl_compositor::Request::CreateSurface { id: surface }
            .encode(self.compositor, writer)
            .map_err(io::Error::other)?;
        xdg_wm_base::Request::GetXdgSurface { id: xdg, surface }
            .encode(self.wm_base, writer)
            .map_err(io::Error::other)?;
        xdg_surface::Request::GetToplevel { id: toplevel }
            .encode(xdg, writer)
            .map_err(io::Error::other)?;
        xdg_toplevel::Request::SetTitle {
            title: title.into(),
        }
        .encode(toplevel, writer)
        .map_err(io::Error::other)?;
        xdg_toplevel::Request::SetAppId {
            app_id: app_id.into(),
        }
        .encode(toplevel, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(surface, writer)
            .map_err(io::Error::other)?;
        let window = WindowId(surface);
        self.windows.insert(
            window,
            Window {
                role: Role::Toplevel {
                    xdg,
                    toplevel,
                    pending: configuration,
                },
                configuration,
                configured: false,
                frame: None,
            },
        );
        self.transport.flush()?;
        Ok(window)
    }

    /// 当前已确认的窗口配置；窗口已销毁或尚未 configure 时返回 `None`。
    pub fn configuration(&self, window: WindowId) -> Option<Configuration> {
        self.windows
            .get(&window)
            .filter(|window| window.configured)
            .map(|window| window.configuration)
    }

    /// 当前实际持有键盘焦点的窗口；应用据此决定是否显示输入光标。
    pub fn keyboard_focus(&self) -> Option<WindowId> {
        self.keyboard
    }

    /// 是否可以开始绘制下一帧；未配置、已销毁或前一帧未完成时返回 false。
    /// 这是连接内 frame callback 状态的投影，应用可据此避免先渲染再遇到 WouldBlock。
    pub fn ready_to_draw(&self, window: WindowId) -> bool {
        self.windows
            .get(&window)
            .is_some_and(|window| window.configured && window.frame.is_none())
    }

    /// 发出窗口管理请求；不直接修改配置，等待 compositor 的后续 Configure。
    ///
    /// # Errors
    ///
    /// 窗口不存在、编码或 socket 失败。
    pub fn request(&mut self, window: WindowId, request: WindowRequest) -> io::Result<()> {
        let toplevel = match self.windows.get(&window).map(|window| &window.role) {
            Some(Role::Toplevel { toplevel, .. }) => *toplevel,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "window request needs an xdg toplevel",
                ));
            }
        };
        let writer = &mut self.transport.writer;
        match request {
            WindowRequest::Move { serial } => xdg_toplevel::Request::Move {
                seat: self.seat,
                serial,
            }
            .encode(toplevel, writer),
            WindowRequest::Resize { serial, edges } => xdg_toplevel::Request::Resize {
                seat: self.seat,
                serial,
                edges,
            }
            .encode(toplevel, writer),
            WindowRequest::Maximize(true) => {
                xdg_toplevel::Request::SetMaximized.encode(toplevel, writer)
            }
            WindowRequest::Maximize(false) => {
                xdg_toplevel::Request::UnsetMaximized.encode(toplevel, writer)
            }
            WindowRequest::Fullscreen(true) => {
                xdg_toplevel::Request::SetFullscreen { output: None }.encode(toplevel, writer)
            }
            WindowRequest::Fullscreen(false) => {
                xdg_toplevel::Request::UnsetFullscreen.encode(toplevel, writer)
            }
            WindowRequest::Minimize => xdg_toplevel::Request::SetMinimized.encode(toplevel, writer),
        }
        .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 更新 toplevel 标题（任务栏与窗口列表显示它）；与客户端自绘的标题栏文字是两件事，调用者需保持一致。
    ///
    /// # Errors
    ///
    /// 窗口不存在、不是 xdg toplevel，或编码/socket 失败。
    pub fn set_title(&mut self, window: WindowId, title: &str) -> io::Result<()> {
        let Some(Role::Toplevel { toplevel, .. }) = self.windows.get(&window).map(|w| &w.role)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set_title needs an xdg toplevel",
            ));
        };
        xdg_toplevel::Request::SetTitle {
            title: title.into(),
        }
        .encode(*toplevel, &mut self.transport.writer)
        .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 销毁一个窗口，撤销输入焦点和重复键；连接里的其他窗口继续存在。
    ///
    /// # Errors
    ///
    /// 窗口不存在或 socket/编码失败。
    pub fn destroy_window(&mut self, window: WindowId) -> io::Result<()> {
        let state = self
            .windows
            .remove(&window)
            .ok_or_else(|| io::Error::other("unknown window"))?;
        if self.pointer == Some(window) {
            self.pointer = None;
        }
        if self.keyboard == Some(window) {
            self.keyboard = None;
            self.repeat = None;
        }
        self.events.retain(|event| event.window != window);
        let writer = &mut self.transport.writer;
        match state.role {
            Role::Toplevel { xdg, toplevel, .. } => {
                xdg_toplevel::Request::Destroy
                    .encode(toplevel, writer)
                    .map_err(io::Error::other)?;
                xdg_surface::Request::Destroy
                    .encode(xdg, writer)
                    .map_err(io::Error::other)?;
            }
            Role::Layer { object } => {
                wayland_wire::protocol::zwlr_layer_surface_v1::Request::Destroy
                    .encode(object, writer)
                    .map_err(io::Error::other)?
            }
        }
        wl_surface::Request::Destroy
            .encode(window.0, writer)
            .map_err(io::Error::other)?;
        self.transport.flush()
    }

    /// 等待一个语义事件；重复键以服务端 repeat_info 为时钟，先消费已排队的真实输入。
    ///
    /// # Errors
    ///
    /// 对端断开、协议、keymap 或 socket 失败。应用应退出并释放 Display，不在坏连接上继续请求。
    pub fn next_event(&mut self) -> io::Result<Event> {
        self.wait_event(None)?
            .ok_or_else(|| io::Error::other("unbounded wait timed out"))
    }

    /// 等待至多 `timeout`，便于会话回收子进程、桌面刷新时钟；超时返回 None。
    /// 协议与连接错误仍返回 Err；重复键与真实输入沿用同一事件队列。
    pub fn next_event_timeout(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        self.wait_event(Some(Instant::now() + timeout))
    }

    fn wait_event(&mut self, deadline: Option<Instant>) -> io::Result<Option<Event>> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if let Some((object, event)) = self.transport.buffered_event()? {
                self.dispatch(object, event)?;
                continue;
            }
            self.transport.flush()?;
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Ok(None);
            }
            let wake = self
                .repeat
                .as_ref()
                .map(|repeat| repeat.next)
                .into_iter()
                .chain(deadline)
                .min();
            // 向上保留不足 1ms，避免 deadline 前忙轮询；所有超时回到顶部重新判断来源。
            let timeout = wake.map(|wake| {
                wake.saturating_duration_since(now)
                    .max(Duration::from_millis(1))
            });
            let mut descriptors = [PollFd::new(self.transport.stream.as_fd(), PollEvents::READ)];
            match poll(&mut descriptors, timeout) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
                Ok(0) => {
                    // 先 poll/read 真正的 release/leave，再生成重复；阻塞操作结束时 socket 可能已有旧键释放。
                    let now = Instant::now();
                    if deadline.is_some_and(|deadline| now >= deadline) {
                        return Ok(None);
                    }
                    if let Some(repeat) = &mut self.repeat
                        && repeat.next <= now
                    {
                        let (window, code) = (repeat.window, repeat.code);
                        repeat.next =
                            now + Duration::from_secs_f64(1.0 / f64::from(self.repeat_rate.max(1)));
                        return Ok(Some(Event {
                            window,
                            kind: EventKind::Key(self.key(code, true, true)),
                        }));
                    }
                }
                Ok(_) => self.transport.read()?,
            }
        }
    }
}
