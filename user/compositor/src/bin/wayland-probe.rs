//! 运行时门禁用的 Wayland 客户端：走完整的 xdg 窗口生命周期并逐帧绘制纯色。
//!
//! 只用于 `scripts/verify_wayland.py`，不进入产品 rootfs。流程：registry → 绑定 → xdg toplevel →
//! 初始 configure/ack → 逐帧 `shm` buffer + `frame` callback → 保持窗口显示，直到被杀。
//! 每个里程碑打印一行 `wayland-probe: ...`，门禁据此判定并随后截图比对窗口像素。

use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::File,
    io::{self, Write},
    os::{
        fd::AsFd,
        unix::{fs::FileExt, net::UnixStream},
    },
    path::PathBuf,
    process::ExitCode,
};

use linux_uapi::{
    shared_memory::MemFd,
    unix::{recv_fds, send_fd},
};
use wayland_wire::{
    protocol::{
        self, AnyEvent, Interface, wl_callback, wl_compositor, wl_display, wl_registry, wl_shm,
        wl_shm_pool, wl_surface, xdg_surface, xdg_toplevel, xdg_wm_base,
    },
    wire::{DynamicNewId, Framer, ObjectId, Reader, Writer},
};

const WIDTH: i32 = 200;
const HEIGHT: i32 = 120;
const FRAMES: u32 = 5;
/// 第 `n` 帧的纯色（XRGB）；门禁用最后一帧的颜色比对屏幕像素。
const FRAME_COLORS: [u32; FRAMES as usize] =
    [0xFF2040C0, 0xFF40C040, 0xFFC04020, 0xFFC0C020, 0xFF20C0C0];

fn id(value: u32) -> ObjectId {
    ObjectId::new(value).expect("probe object ids are non-zero")
}

struct Connection {
    stream: UnixStream,
    framer: Framer,
    fds: VecDeque<std::os::fd::OwnedFd>,
    writer: Writer,
    interfaces: HashMap<u32, Interface>,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        let interfaces = HashMap::from([(1, Interface::WlDisplay)]);
        Self {
            stream,
            framer: Framer::new(),
            fds: VecDeque::new(),
            writer: Writer::new(),
            interfaces,
        }
    }

    /// 登记新对象的接口，以便解码它的事件。
    fn track(&mut self, object: u32, interface: Interface) -> ObjectId {
        self.interfaces.insert(object, interface);
        id(object)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.write_all(self.writer.bytes())?;
        let sent = self.writer.bytes().len();
        self.writer.consume(sent, 0);
        Ok(())
    }

    fn next_event(&mut self) -> io::Result<(ObjectId, AnyEvent)> {
        loop {
            if let Some(frame) = self.framer.next_frame().map_err(io::Error::other)? {
                let interface = *self.interfaces.get(&frame.object.get()).ok_or_else(|| {
                    io::Error::other(format!("event for unknown object {}", frame.object.get()))
                })?;
                let event = protocol::decode_event(
                    interface,
                    frame.opcode,
                    &mut Reader::new(&frame.body, &mut self.fds),
                )
                .map_err(io::Error::other)?;
                return Ok((frame.object, event));
            }
            let mut buffer = [0u8; 4096];
            let (count, fds) = recv_fds(self.stream.as_fd(), &mut buffer)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "compositor closed the connection",
                ));
            }
            self.framer.push(&buffer[..count]);
            self.fds.extend(fds);
        }
    }

    /// 等待 `callback` 的 done；沿途把其他事件交给 `observe`。
    fn wait_callback(
        &mut self,
        callback: ObjectId,
        mut observe: impl FnMut(ObjectId, AnyEvent),
    ) -> io::Result<()> {
        loop {
            let (object, event) = self.next_event()?;
            if object == callback
                && matches!(event, AnyEvent::WlCallback(wl_callback::Event::Done { .. }))
            {
                return Ok(());
            }
            if let AnyEvent::WlDisplay(wl_display::Event::Error { code, message, .. }) = &event {
                return Err(io::Error::other(format!(
                    "protocol error {code}: {message}"
                )));
            }
            observe(object, event);
        }
    }
}

fn run() -> io::Result<()> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    let mut connection = Connection::new(UnixStream::connect(
        PathBuf::from(runtime).join("wayland-0"),
    )?);

    let registry = connection.track(2, Interface::WlRegistry);
    wl_display::Request::GetRegistry { registry }
        .encode(ObjectId::DISPLAY, &mut connection.writer)
        .map_err(io::Error::other)?;
    let sync = connection.track(3, Interface::WlCallback);
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

    let mut next = 4;
    let mut bind = |connection: &mut Connection, interface: Interface| -> io::Result<ObjectId> {
        let (name, version) = *globals.get(interface.name()).ok_or_else(|| {
            io::Error::other(format!(
                "compositor does not advertise {}",
                interface.name()
            ))
        })?;
        let object = connection.track(next, interface);
        next += 1;
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
    println!("wayland-probe: bound compositor shm xdg_wm_base output");

    let surface = connection.track(20, Interface::WlSurface);
    wl_compositor::Request::CreateSurface { id: surface }
        .encode(compositor, &mut connection.writer)
        .map_err(io::Error::other)?;
    let xdg = connection.track(21, Interface::XdgSurface);
    xdg_wm_base::Request::GetXdgSurface { id: xdg, surface }
        .encode(wm_base, &mut connection.writer)
        .map_err(io::Error::other)?;
    let toplevel = connection.track(22, Interface::XdgToplevel);
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

    // 初始 configure：必须确认之后才能附着 buffer。
    let serial = loop {
        let (_, event) = connection.next_event()?;
        match event {
            AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => break serial,
            AnyEvent::WlDisplay(wl_display::Event::Error { code, message, .. }) => {
                return Err(io::Error::other(format!(
                    "protocol error {code}: {message}"
                )));
            }
            _ => {}
        }
    };
    xdg_surface::Request::AckConfigure { serial }
        .encode(xdg, &mut connection.writer)
        .map_err(io::Error::other)?;
    println!("wayland-probe: configured serial={serial}");

    let stride = WIDTH * 4;
    let size = (stride * HEIGHT) as usize;
    let memory = MemFd::create("wayland-probe", size)?;
    let file = File::from(memory.as_fd().try_clone_to_owned()?);
    let pool = connection.track(23, Interface::WlShmPool);
    // 描述符随 create_pool 一起经 SCM_RIGHTS 发送；先发完前面排队的字节，保证顺序。
    connection.flush()?;
    let mut request = Writer::new();
    request.begin(shm, 0);
    request.object(Some(pool));
    request.int(size as i32);
    request.end().map_err(io::Error::other)?;
    send_fd(connection.stream.as_fd(), request.bytes(), memory.as_fd())?;
    println!("wayland-probe: pool {size} bytes");

    for frame in 0..FRAMES {
        let color = FRAME_COLORS[frame as usize];
        let row: Vec<u8> = std::iter::repeat_n(color.to_le_bytes(), WIDTH as usize)
            .flatten()
            .collect();
        for y in 0..HEIGHT as u64 {
            file.write_all_at(&row, y * stride as u64)?;
        }
        let buffer = connection.track(30 + frame, Interface::WlBuffer);
        wl_shm_pool::Request::CreateBuffer {
            id: buffer,
            offset: 0,
            width: WIDTH,
            height: HEIGHT,
            stride,
            format: wl_shm::Format::Xrgb8888,
        }
        .encode(pool, &mut connection.writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Attach {
            buffer: Some(buffer),
            x: 0,
            y: 0,
        }
        .encode(surface, &mut connection.writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::Damage {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        }
        .encode(surface, &mut connection.writer)
        .map_err(io::Error::other)?;
        let callback = connection.track(60 + frame, Interface::WlCallback);
        wl_surface::Request::Frame { callback }
            .encode(surface, &mut connection.writer)
            .map_err(io::Error::other)?;
        wl_surface::Request::Commit
            .encode(surface, &mut connection.writer)
            .map_err(io::Error::other)?;
        connection.flush()?;
        connection.wait_callback(callback, |_, _| {})?;
        println!("wayland-probe: frame {frame} presented color={color:08x}");
    }
    println!("wayland-probe: done");
    // 窗口必须保持映射，门禁才能截图；之后只等待连接被关闭。
    loop {
        if let (_, AnyEvent::XdgToplevel(xdg_toplevel::Event::Close)) = connection.next_event()? {
            println!("wayland-probe: close requested");
            return Ok(());
        }
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
