//! 运行时门禁用的 Wayland 客户端：走完整的 xdg 窗口生命周期，逐帧绘制纯色，并像带客户端装饰的窗口那样发窗口请求。
//!
//! 只用于 `scripts/verify_wayland.py`，不进入产品 rootfs。流程：registry → 绑定 → xdg toplevel →
//! 初始 configure/ack → 逐帧 `shm` buffer + `frame` callback → 保持窗口显示并响应输入：
//! 在窗口上部（"标题栏"）按下 → `xdg_toplevel.move`；右下角按下 → `resize`；按键 m/f/n → 最大化/全屏/最小化切换；
//! 收到新的 configure 尺寸就按新尺寸重绘。
//! 每个里程碑打印一行 `wayland-probe: ...`，门禁据此判定并随后截图比对窗口像素。

#[path = "../probe_connection.rs"]
mod probe_connection;
use probe_connection::Connection;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    process::ExitCode,
};
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_callback, wl_compositor, wl_display, wl_keyboard, wl_pointer,
        wl_registry, wl_seat, wl_surface, xdg_surface, xdg_toplevel, xdg_wm_base,
    },
    wire::{DynamicNewId, ObjectId},
};

/// 收到的 keymap 写到这里，guest 里的 `xkb-check` 用上游 libxkbcommon 编译它。
const KEYMAP_PATH: &str = "/var/tmp/keymap.xkb";
const WIDTH: i32 = 200;
const HEIGHT: i32 = 120;
const FRAMES: u32 = 5;
/// 第 `n` 帧的纯色（XRGB）；门禁用最后一帧的颜色比对屏幕像素。
const FRAME_COLORS: [u32; FRAMES as usize] =
    [0xFF2040C0, 0xFF40C040, 0xFFC04020, 0xFFC0C020, 0xFF20C0C0];

/// 窗口上部这么多像素内按下鼠标视为点在标题栏上。
const TITLE_HEIGHT: i32 = 30;
/// 右下角这么大的方块内按下视为抓住缩放角。
const CORNER: i32 = 12;
const KEY_N: u32 = 49;
const KEY_M: u32 = 50;
const KEY_F: u32 = 33;
const BOTTOM_RIGHT: xdg_toplevel::ResizeEdge = xdg_toplevel::ResizeEdge::BottomRight;

/// 把 `configure` 的状态数组解码成可读名字。
fn state_names(states: &[u8]) -> Vec<&'static str> {
    states
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| match u32::from_ne_bytes(*chunk) {
            1 => "maximized",
            2 => "fullscreen",
            3 => "resizing",
            4 => "activated",
            _ => "unknown",
        })
        .collect()
}

struct Probe {
    connection: Connection,
    shm: ObjectId,
    surface: ObjectId,
    xdg: ObjectId,
    toplevel: ObjectId,
    seat: ObjectId,
    size: (i32, i32),
    color: u32,
    /// 最近一次 configure 带来的非零目标尺寸，在 `xdg_surface.configure` 确认后生效。
    pending_size: Option<(i32, i32)>,
    maximized: bool,
    fullscreen: bool,
    /// 指针在窗口内的局部整数坐标。
    pointer: (i32, i32),
}

impl Probe {
    fn new_object(&mut self, interface: Interface) -> ObjectId {
        self.connection.new_object(interface)
    }

    fn send(&mut self, request: Result<(), wayland_wire::wire::EncodeError>) -> io::Result<()> {
        request.map_err(io::Error::other)
    }

    /// 用纯色重绘整个窗口：新建 memfd 池与 buffer，附着并提交，随后销毁它们（服务端在 commit 时已复制）。
    fn draw(&mut self, color: u32, frame: Option<ObjectId>) -> io::Result<()> {
        self.connection
            .draw_solid(self.shm, self.surface, self.size, color, 1, frame)?;
        self.color = color;
        Ok(())
    }

    /// 处理一个事件：打印里程碑，并对 configure、按钮与按键做出像真实客户端一样的反应。
    fn handle(&mut self, event: AnyEvent) -> io::Result<()> {
        match event {
            AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            }) => {
                self.pending_size = (width > 0 && height > 0).then_some((width, height));
                self.maximized = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_ne_bytes(*c) == 1);
                self.fullscreen = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_ne_bytes(*c) == 2);
                println!(
                    "wayland-probe: configure {width}x{height} states={:?}",
                    state_names(&states)
                );
            }
            AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => {
                let ack = xdg_surface::Request::AckConfigure { serial }
                    .encode(self.xdg, &mut self.connection.writer);
                self.send(ack)?;
                match self.pending_size.take() {
                    Some(size) if size != self.size => {
                        self.size = size;
                        self.draw(self.color, None)?;
                        println!("wayland-probe: resized {}x{}", size.0, size.1);
                    }
                    _ => self.connection.flush()?,
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }) => {
                self.pointer = (surface_x.to_int(), surface_y.to_int());
                println!(
                    "wayland-probe: pointer enter x={} y={}",
                    self.pointer.0, self.pointer.1
                );
            }
            AnyEvent::WlPointer(wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            }) => {
                self.pointer = (surface_x.to_int(), surface_y.to_int());
                observe(AnyEvent::WlPointer(wl_pointer::Event::Motion {
                    time,
                    surface_x,
                    surface_y,
                }));
            }
            AnyEvent::WlPointer(wl_pointer::Event::Button {
                serial,
                button,
                state,
                ..
            }) => {
                observe(AnyEvent::WlPointer(wl_pointer::Event::Button {
                    serial,
                    time: 0,
                    button,
                    state,
                }));
                if state == wl_pointer::ButtonState::Pressed {
                    let (x, y) = self.pointer;
                    let request = if y < TITLE_HEIGHT {
                        Some(xdg_toplevel::Request::Move {
                            seat: self.seat,
                            serial,
                        })
                    } else if x >= self.size.0 - CORNER && y >= self.size.1 - CORNER {
                        Some(xdg_toplevel::Request::Resize {
                            seat: self.seat,
                            serial,
                            edges: BOTTOM_RIGHT,
                        })
                    } else {
                        None
                    };
                    if let Some(request) = request {
                        let encoded = request.encode(self.toplevel, &mut self.connection.writer);
                        self.send(encoded)?;
                        self.connection.flush()?;
                        println!("wayland-probe: requested window drag at {x},{y}");
                    }
                }
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
                key,
                state: wl_keyboard::KeyState::Pressed,
                ..
            }) if matches!(key, KEY_M | KEY_F | KEY_N) => {
                let request = match key {
                    KEY_M if self.maximized => xdg_toplevel::Request::UnsetMaximized,
                    KEY_M => xdg_toplevel::Request::SetMaximized,
                    KEY_F if self.fullscreen => xdg_toplevel::Request::UnsetFullscreen,
                    KEY_F => xdg_toplevel::Request::SetFullscreen { output: None },
                    _ => xdg_toplevel::Request::SetMinimized,
                };
                let encoded = request.encode(self.toplevel, &mut self.connection.writer);
                self.send(encoded)?;
                self.connection.flush()?;
                println!("wayland-probe: requested {request:?}");
            }
            other => observe(other),
        }
        Ok(())
    }

    /// 等待 `callback` 的 done，沿途处理其他事件；协议错误终止探针。
    fn wait_callback(&mut self, callback: ObjectId) -> io::Result<()> {
        loop {
            let (object, event) = self.connection.next_event()?;
            if object == callback
                && matches!(event, AnyEvent::WlCallback(wl_callback::Event::Done { .. }))
            {
                return Ok(());
            }
            self.check_error(&event)?;
            self.handle(event)?;
        }
    }

    fn check_error(&self, event: &AnyEvent) -> io::Result<()> {
        match event {
            AnyEvent::WlDisplay(wl_display::Event::Error { code, message, .. }) => Err(
                io::Error::other(format!("protocol error {code}: {message}")),
            ),
            _ => Ok(()),
        }
    }
}

/// 把 seat 事件打印成门禁识别的里程碑；其余事件忽略。
fn observe(event: AnyEvent) {
    match event {
        AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { fd, size, .. }) => {
            let mut text = Vec::new();
            let saved = File::from(fd)
                .read_to_end(&mut text)
                .and_then(|_| fs::write(KEYMAP_PATH, &text));
            match saved {
                Ok(()) => println!("wayland-probe: keymap {size} bytes"),
                Err(error) => println!("wayland-probe: keymap error {error}"),
            }
        }
        AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { .. }) => {
            println!("wayland-probe: keyboard enter")
        }
        AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. }) => {
            println!("wayland-probe: keyboard leave")
        }
        AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. }) => {
            println!(
                "wayland-probe: key {key} {}",
                format!("{state:?}").to_lowercase()
            );
        }
        AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers {
            mods_depressed,
            mods_locked,
            ..
        }) => {
            println!("wayland-probe: modifiers depressed={mods_depressed} locked={mods_locked}");
        }
        AnyEvent::WlPointer(wl_pointer::Event::Leave { .. }) => {
            println!("wayland-probe: pointer leave")
        }
        AnyEvent::WlPointer(wl_pointer::Event::Motion {
            surface_x,
            surface_y,
            ..
        }) => {
            println!(
                "wayland-probe: pointer motion x={} y={}",
                surface_x.to_int(),
                surface_y.to_int()
            );
        }
        AnyEvent::WlPointer(wl_pointer::Event::Button { button, state, .. }) => {
            println!(
                "wayland-probe: pointer button {button} {}",
                format!("{state:?}").to_lowercase()
            );
        }
        _ => {}
    }
}

fn run() -> io::Result<()> {
    let mut connection = Connection::connect()?;
    let registry = connection.new_object(Interface::WlRegistry);
    wl_display::Request::GetRegistry { registry }
        .encode(ObjectId::DISPLAY, &mut connection.writer)
        .map_err(io::Error::other)?;
    let sync = connection.new_object(Interface::WlCallback);
    wl_display::Request::Sync { callback: sync }
        .encode(ObjectId::DISPLAY, &mut connection.writer)
        .map_err(io::Error::other)?;
    connection.flush()?;
    let mut globals = HashMap::new();
    connection.wait_callback(sync, |_, event| {
        if let AnyEvent::WlRegistry(wl_registry::Event::Global {
            name,
            interface,
            version,
        }) = event
        {
            globals.insert(interface, (name, version));
        }
    })?;

    let bind = |connection: &mut Connection, interface: Interface| -> io::Result<ObjectId> {
        let (name, version) = *globals.get(interface.name()).ok_or_else(|| {
            io::Error::other(format!(
                "compositor does not advertise {}",
                interface.name()
            ))
        })?;
        let object = connection.new_object(interface);
        let request = wl_registry::Request::Bind {
            name,
            id: DynamicNewId {
                interface: interface.name().to_owned(),
                version: version.min(interface.version()),
                id: object,
            },
        };
        request
            .encode(registry, &mut connection.writer)
            .map_err(io::Error::other)?;
        Ok(object)
    };
    let compositor = bind(&mut connection, Interface::WlCompositor)?;
    let shm = bind(&mut connection, Interface::WlShm)?;
    let wm_base = bind(&mut connection, Interface::XdgWmBase)?;
    let _output = bind(&mut connection, Interface::WlOutput)?;
    let seat = bind(&mut connection, Interface::WlSeat)?;
    let pointer = connection.new_object(Interface::WlPointer);
    wl_seat::Request::GetPointer { id: pointer }
        .encode(seat, &mut connection.writer)
        .map_err(io::Error::other)?;
    let keyboard = connection.new_object(Interface::WlKeyboard);
    wl_seat::Request::GetKeyboard { id: keyboard }
        .encode(seat, &mut connection.writer)
        .map_err(io::Error::other)?;
    println!("wayland-probe: bound compositor shm xdg_wm_base output seat");

    let surface = connection.new_object(Interface::WlSurface);
    wl_compositor::Request::CreateSurface { id: surface }
        .encode(compositor, &mut connection.writer)
        .map_err(io::Error::other)?;
    let xdg = connection.new_object(Interface::XdgSurface);
    xdg_wm_base::Request::GetXdgSurface { id: xdg, surface }
        .encode(wm_base, &mut connection.writer)
        .map_err(io::Error::other)?;
    let toplevel = connection.new_object(Interface::XdgToplevel);
    xdg_surface::Request::GetToplevel { id: toplevel }
        .encode(xdg, &mut connection.writer)
        .map_err(io::Error::other)?;
    xdg_toplevel::Request::SetTitle {
        title: "Wayland probe".into(),
    }
    .encode(toplevel, &mut connection.writer)
    .map_err(io::Error::other)?;
    xdg_toplevel::Request::SetAppId {
        app_id: "wayland-probe".into(),
    }
    .encode(toplevel, &mut connection.writer)
    .map_err(io::Error::other)?;
    wl_surface::Request::Commit
        .encode(surface, &mut connection.writer)
        .map_err(io::Error::other)?;
    connection.flush()?;

    let mut probe = Probe {
        connection,
        shm,
        surface,
        xdg,
        toplevel,
        seat,
        size: (WIDTH, HEIGHT),
        color: FRAME_COLORS[0],
        pending_size: None,
        maximized: false,
        fullscreen: false,
        pointer: (0, 0),
    };
    // 初始 configure：必须确认之后才能附着 buffer。
    loop {
        let (_, event) = probe.connection.next_event()?;
        probe.check_error(&event)?;
        let is_initial = matches!(
            event,
            AnyEvent::XdgSurface(xdg_surface::Event::Configure { .. })
        );
        probe.handle(event)?;
        if is_initial {
            break;
        }
    }
    println!("wayland-probe: configured");

    for frame in 0..FRAMES {
        let color = FRAME_COLORS[frame as usize];
        let callback = probe.new_object(Interface::WlCallback);
        probe.draw(color, Some(callback))?;
        probe.wait_callback(callback)?;
        println!("wayland-probe: frame {frame} presented color={color:08x}");
    }
    println!("wayland-probe: done");
    // 窗口必须保持映射，门禁才能截图；之后响应输入与 configure，直到被要求关闭。
    loop {
        let (_, event) = probe.connection.next_event()?;
        probe.check_error(&event)?;
        if matches!(event, AnyEvent::XdgToplevel(xdg_toplevel::Event::Close)) {
            println!("wayland-probe: close requested");
            return Ok(());
        }
        probe.handle(event)?;
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wayland-probe: error: {error}");
            ExitCode::FAILURE
        }
    }
}
