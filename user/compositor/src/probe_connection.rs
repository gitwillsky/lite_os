//! 两个门禁探针（`wayland-probe`、`shell-probe`）共用的最小 Wayland 客户端连接：不属于 compositor 本身，
//! 只被各探针以 `#[path]` 包含。

use std::{
    collections::{HashMap, VecDeque},
    env,
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::{fs::FileExt, net::UnixStream},
    },
    path::PathBuf,
};

use linux_uapi::{
    process::wayland_socket_from_env,
    shared_memory::MemFd,
    unix::{recv_fds, send_fd},
};
use wayland_wire::{
    protocol::{
        self, AnyEvent, Interface, wl_buffer, wl_callback, wl_display, wl_shm, wl_shm_pool,
        wl_surface,
    },
    wire::{Framer, ObjectId, Reader, Writer},
};

pub fn id(value: u32) -> ObjectId {
    ObjectId::new(value).expect("probe object ids are non-zero")
}

pub struct Connection {
    pub stream: UnixStream,
    framer: Framer,
    fds: VecDeque<OwnedFd>,
    pub writer: Writer,
    interfaces: HashMap<u32, Interface>,
    /// 下一个自动分配的对象 id。
    next_id: u32,
}

impl Connection {
    /// 连接 compositor：特权外壳用继承的 `WAYLAND_SOCKET`，普通客户端用 `$XDG_RUNTIME_DIR/wayland-0`。
    pub fn connect() -> io::Result<Self> {
        let stream = match wayland_socket_from_env()? {
            Some(stream) => stream,
            None => {
                let runtime = env::var_os("XDG_RUNTIME_DIR")
                    .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
                UnixStream::connect(PathBuf::from(runtime).join("wayland-0"))?
            }
        };
        Ok(Self {
            stream,
            framer: Framer::new(),
            fds: VecDeque::new(),
            writer: Writer::new(),
            interfaces: HashMap::from([(1, Interface::WlDisplay)]),
            next_id: 2,
        })
    }

    /// 分配一个新对象 id 并登记它的接口，以便解码它的事件。
    pub fn new_object(&mut self, interface: Interface) -> ObjectId {
        let object = self.next_id;
        self.next_id += 1;
        self.adopt(id(object), interface);
        id(object)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.stream.write_all(self.writer.bytes())?;
        let sent = self.writer.bytes().len();
        self.writer.consume(sent, 0);
        Ok(())
    }

    pub fn next_event(&mut self) -> io::Result<(ObjectId, AnyEvent)> {
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
                if let AnyEvent::WlDisplay(wl_display::Event::Error { code, message, .. }) = &event
                {
                    return Err(io::Error::other(format!(
                        "protocol error {code}: {message}"
                    )));
                }
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

    /// 登记一个服务端分配的对象（如 foreign-toplevel 句柄）的接口。
    pub fn adopt(&mut self, object: ObjectId, interface: Interface) {
        self.interfaces.insert(object.get(), interface);
    }

    /// 等待 `callback` 的 done；沿途把其他事件交给 `observe`。
    pub fn wait_callback(
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
            observe(object, event);
        }
    }

    /// 用纯色填满 `size`（物理像素）并提交到 `surface`：新建 memfd 池与 buffer，附着、提交后销毁它们。
    ///
    /// # Parameters
    ///
    /// - `scale`: 设置的 buffer scale；`frame` 是随这次提交请求的 frame callback。
    pub fn draw_solid(
        &mut self,
        shm: ObjectId,
        surface: ObjectId,
        size: (i32, i32),
        color: u32,
        scale: i32,
        frame: Option<ObjectId>,
    ) -> io::Result<()> {
        let (width, height) = size;
        let stride = width * 4;
        let bytes = (stride * height) as usize;
        let memory = MemFd::create("probe", bytes)?;
        let file = File::from(memory.as_fd().try_clone_to_owned()?);
        let row: Vec<u8> = std::iter::repeat_n(color.to_le_bytes(), width as usize)
            .flatten()
            .collect();
        for y in 0..height as u64 {
            file.write_all_at(&row, y * stride as u64)?;
        }
        let pool = self.new_object(Interface::WlShmPool);
        // 描述符随 create_pool 一起经 SCM_RIGHTS 发送；先发完前面排队的字节，保证顺序。
        self.flush()?;
        let mut request = Writer::new();
        request.begin(shm, 0);
        request.object(Some(pool));
        request.int(bytes as i32);
        request.end().map_err(io::Error::other)?;
        send_fd(self.stream.as_fd(), request.bytes(), memory.as_fd())?;
        let buffer = self.new_object(Interface::WlBuffer);
        let writer = &mut self.writer;
        let mut requests = vec![
            wl_shm_pool::Request::CreateBuffer {
                id: buffer,
                offset: 0,
                width,
                height,
                stride,
                format: wl_shm::Format::Argb8888,
            }
            .encode(pool, writer),
            wl_surface::Request::SetBufferScale { scale }.encode(surface, writer),
            wl_surface::Request::Attach {
                buffer: Some(buffer),
                x: 0,
                y: 0,
            }
            .encode(surface, writer),
            wl_surface::Request::Damage {
                x: 0,
                y: 0,
                width,
                height,
            }
            .encode(surface, writer),
        ];
        if let Some(callback) = frame {
            requests.push(wl_surface::Request::Frame { callback }.encode(surface, writer));
        }
        requests.push(wl_surface::Request::Commit.encode(surface, writer));
        requests.push(wl_buffer::Request::Destroy.encode(buffer, writer));
        requests.push(wl_shm_pool::Request::Destroy.encode(pool, writer));
        for request in requests {
            request.map_err(io::Error::other)?;
        }
        self.flush()
    }
}
