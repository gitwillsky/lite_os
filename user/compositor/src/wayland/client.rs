//! 一个 Wayland 连接的协议状态机：对象表、请求分发与协议错误。

use std::{
    collections::{HashMap, VecDeque},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    rc::Rc,
};

use wayland_wire::{
    protocol::{
        self, AnyRequest, Interface, wl_buffer, wl_callback, wl_compositor, wl_display, wl_output,
        wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    wire::{DecodeError, EncodeError, Framer, ObjectId, Reader, Writer},
};

use super::{
    Output,
    foreign::{ForeignHandle, ForeignManager, ForeignRequest, SERVER_ID_BASE},
    layer::LayerSurface,
    seat::CursorRequest,
    shm::{BufferSpec, Pool},
    surface::{Content, Surface},
    xdg::{Toplevel, WindowRequest, XdgSurface},
};

/// 未消费的描述符上限：恶意客户端只发 fd 不发消息时，队列不能无限增长。
const MAX_QUEUED_FDS: usize = 64;

/// 已发出协议错误或收到无法同步的数据；连接必须关闭（先发完 [`Client::output`]）。
#[derive(Debug, PartialEq, Eq)]
pub struct Disconnected;

/// 一个协议错误：发给 `wl_display.error` 后断开。
pub(super) struct Fail {
    pub(super) object: ObjectId,
    pub(super) code: u32,
    pub(super) message: String,
}

impl Fail {
    pub(super) fn new(object: ObjectId, code: u32, message: impl Into<String>) -> Self {
        Self {
            object,
            code,
            message: message.into(),
        }
    }

    pub(super) fn invalid_object(message: impl Into<String>) -> Self {
        Self::new(
            ObjectId::DISPLAY,
            wl_display::Error::InvalidObject as u32,
            message,
        )
    }

    pub(super) fn invalid_method(object: ObjectId, message: impl Into<String>) -> Self {
        Self::new(object, wl_display::Error::InvalidMethod as u32, message)
    }

    pub(super) fn implementation(object: ObjectId) -> Self {
        Self::new(
            object,
            wl_display::Error::Implementation as u32,
            "event encoding failed",
        )
    }
}

impl From<EncodeError> for Fail {
    fn from(_: EncodeError) -> Self {
        Self::implementation(ObjectId::DISPLAY)
    }
}

pub(super) enum Data {
    None,
    Pool(Rc<Pool>),
    Buffer(BufferSpec),
    Surface(Surface),
    XdgSurface(XdgSurface),
    Toplevel(Toplevel),
    LayerSurface(LayerSurface),
    SecurityContext(super::security::Context),
    ForeignHandle(ForeignHandle),
}

pub(super) struct Entry {
    pub(super) interface: Interface,
    pub(super) version: u32,
    pub(super) data: Data,
}

/// 向客户端公布的全局对象：`(name, 接口, 最高版本)`。
const GLOBALS: [(u32, Interface, u32); 5] = [
    (1, Interface::WlCompositor, 6),
    (2, Interface::WlShm, 1),
    (3, Interface::XdgWmBase, 3),
    (4, Interface::WlOutput, 4),
    (5, Interface::WlSeat, 7),
];

/// 初始 Session 与通过 security-context 委派的 Shell 可见的全局对象：
/// 桌面外壳需要它们，普通应用不能借此伪装成任务栏或枚举别人的窗口。
const PRIVILEGED_GLOBALS: [(u32, Interface, u32); 2] = [
    (6, Interface::ZwlrLayerShellV1, 5),
    (7, Interface::ZwlrForeignToplevelManagerV1, 3),
];

/// 连接能力只由 Server 的来源路径选择，不从 uid 或客户端 metadata 推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Privilege {
    Application,
    Session,
    Shell,
}

pub struct Client {
    framer: Framer,
    fds: VecDeque<OwnedFd>,
    pub(super) objects: HashMap<u32, Entry>,
    pub(super) writer: Writer,
    dead: bool,
    /// 下一个 `xdg_surface.configure` 序号；每个连接独立。
    pub(super) next_serial: u32,
    pub(super) requests: Vec<(ObjectId, WindowRequest)>,
    commits: u64,
    pub(super) output: Output,
    /// 来源决定 Application/Session/Shell 能力；不从 uid 或客户端提供的 metadata 推断。
    pub(super) privilege: Privilege,
    pub(super) security_contexts: Vec<super::security::Listener>,
    /// 外壳绑定的窗口管理器及其窗口列表（特权连接才会有）。
    pub(super) foreign: Vec<ForeignManager>,
    pub(super) foreign_requests: Vec<ForeignRequest>,
    pub(super) cursor_requests: Vec<CursorRequest>,
    /// 下一个服务端分配的对象 id（窗口句柄）。
    pub(super) next_server_object: u32,
    /// 随消息发送的描述符：`(在待发字节中的偏移, fd)`，偏移按发送顺序递增。
    pub(super) outgoing_fds: VecDeque<(usize, OwnedFd)>,
}

impl Client {
    /// 普通客户端。
    pub fn new(output: Output) -> Self {
        Self::with_privilege(output, Privilege::Application)
    }

    /// 初始会话客户端：拥有外壳能力，并可通过 security-context 委派独立桌面连接。
    pub fn session(output: Output) -> Self {
        Self::with_privilege(output, Privilege::Session)
    }

    pub(super) fn shell(output: Output) -> Self {
        Self::with_privilege(output, Privilege::Shell)
    }

    fn with_privilege(output: Output, privilege: Privilege) -> Self {
        let display = Entry {
            interface: Interface::WlDisplay,
            version: 1,
            data: Data::None,
        };
        Self {
            framer: Framer::new(),
            fds: VecDeque::new(),
            objects: HashMap::from([(ObjectId::DISPLAY.get(), display)]),
            writer: Writer::new(),
            dead: false,
            next_serial: 1,
            requests: Vec::new(),
            commits: 0,
            output,
            privilege,
            security_contexts: Vec::new(),
            foreign: Vec::new(),
            foreign_requests: Vec::new(),
            cursor_requests: Vec::new(),
            next_server_object: SERVER_ID_BASE,
            outgoing_fds: VecDeque::new(),
        }
    }

    /// 喂入从 socket 读到的字节与随之到达的描述符，处理其中所有完整消息。
    ///
    /// # Errors
    ///
    /// 协议错误或无法同步的数据。此时 `wl_display.error`（若适用）已写入 [`Client::output`]，
    /// 调用者应发完输出后关闭连接；之后的 `receive` 一律返回 `Err`。
    pub fn receive(&mut self, bytes: &[u8], fds: Vec<OwnedFd>) -> Result<(), Disconnected> {
        if self.dead {
            return Err(Disconnected);
        }
        self.framer.push(bytes);
        self.fds.extend(fds);
        if self.fds.len() > MAX_QUEUED_FDS {
            return self.fatal(Fail::invalid_method(
                ObjectId::DISPLAY,
                "too many unread file descriptors",
            ));
        }
        loop {
            let frame = match self.framer.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(()),
                Err(error) => {
                    return self.fatal(Fail::invalid_method(ObjectId::DISPLAY, error.to_string()));
                }
            };
            if let Err(fail) = self.dispatch(frame.object, frame.opcode, &frame.body) {
                return self.fatal(fail);
            }
        }
    }

    fn fatal(&mut self, fail: Fail) -> Result<(), Disconnected> {
        self.dead = true;
        let event = wl_display::Event::Error {
            object_id: fail.object,
            code: fail.code,
            message: fail.message,
        };
        // 错误消息本身超长被拒绝时无事可做：连接照样关闭。
        let _ = event.encode(ObjectId::DISPLAY, &mut self.writer);
        Err(Disconnected)
    }

    /// 下一段可以一次发出的字节，以及必须随这段第一个字节发送的描述符（若有）。
    ///
    /// 段在下一个描述符所在消息之前截断：描述符必须先于引用它的消息到达，
    /// 否则客户端解析到该消息时 fd 队列为空，会判定连接损坏。
    pub fn output(&self) -> (&[u8], Option<BorrowedFd<'_>>) {
        let bytes = self.writer.bytes();
        match self.outgoing_fds.front() {
            Some((0, fd)) => {
                let end = self
                    .outgoing_fds
                    .get(1)
                    .map_or(bytes.len(), |(offset, _)| *offset);
                (&bytes[..end], Some(fd.as_fd()))
            }
            Some((offset, _)) => (&bytes[..*offset], None),
            None => (bytes, None),
        }
    }

    /// 发送成功后丢弃已发出的前 `bytes` 个字节；该段若带描述符，描述符随之完成使命。
    pub fn consume_output(&mut self, bytes: usize) {
        if bytes > 0 && matches!(self.outgoing_fds.front(), Some((0, _))) {
            self.outgoing_fds.pop_front();
        }
        for (offset, _) in &mut self.outgoing_fds {
            *offset -= bytes;
        }
        self.writer.consume(bytes, 0);
    }

    /// 本帧已呈现：向所有已提交的 `wl_surface.frame` 发 `done(time_ms)` 并回收这些 callback。
    pub fn finish_frame(&mut self, time_ms: u32) {
        let callbacks: Vec<ObjectId> = self
            .objects
            .values_mut()
            .filter_map(|entry| match &mut entry.data {
                Data::Surface(surface) => Some(std::mem::take(&mut surface.queued_callbacks)),
                _ => None,
            })
            .flatten()
            .collect();
        for callback in callbacks {
            // 编码只会因超长失败，callback_data 是定长。
            let _ = wl_callback::Event::Done {
                callback_data: time_ms,
            }
            .encode(callback, &mut self.writer);
            self.release_id(callback);
        }
    }

    /// socket 已不可写时丢弃待发字节与 fd，允许 Server 立即移除坏连接，不在 HUP 上反复尝试刷新。
    pub(super) fn discard_output(&mut self) {
        self.writer = Writer::new();
        self.outgoing_fds.clear();
    }

    /// 本连接已处理的 `wl_surface.commit` 次数。
    pub fn commits(&self) -> u64 {
        self.commits
    }

    /// 全部已映射 surface 的当前内容（不论角色）。
    #[cfg(test)]
    pub fn contents(&self) -> impl Iterator<Item = (ObjectId, &Content)> {
        self.objects
            .iter()
            .filter_map(|(id, entry)| match &entry.data {
                Data::Surface(Surface {
                    content: Some(content),
                    ..
                }) => Some((ObjectId::new(*id)?, content)),
                _ => None,
            })
    }

    /// 移除对象并通知客户端可以复用该 id。
    pub(super) fn release_id(&mut self, id: ObjectId) {
        self.objects.remove(&id.get());
        // `delete_id` 只用于客户端分配的 id；服务端分配的 id（窗口句柄）由客户端自己回收，
        // 对它发 delete_id 会让 libwayland 判定协议错误。
        if id.get() < SERVER_ID_BASE {
            let _ = wl_display::Event::DeleteId { id: id.get() }
                .encode(ObjectId::DISPLAY, &mut self.writer);
        }
    }

    pub(super) fn create(
        &mut self,
        id: ObjectId,
        interface: Interface,
        version: u32,
        data: Data,
    ) -> Result<(), Fail> {
        if id.get() >= SERVER_ID_BASE || self.objects.contains_key(&id.get()) {
            return Err(Fail::invalid_object(format!(
                "new id {} is invalid or in use",
                id.get()
            )));
        }
        self.objects.insert(
            id.get(),
            Entry {
                interface,
                version,
                data,
            },
        );
        Ok(())
    }

    fn dispatch(&mut self, object: ObjectId, opcode: u16, body: &[u8]) -> Result<(), Fail> {
        let entry = self
            .objects
            .get(&object.get())
            .ok_or_else(|| Fail::invalid_object(format!("unknown object {}", object.get())))?;
        let (interface, version) = (entry.interface, entry.version);
        let request = {
            let mut reader = Reader::new(body, &mut self.fds);
            protocol::decode_request(interface, opcode, &mut reader)
                .map_err(|error: DecodeError| Fail::invalid_method(object, error.to_string()))?
        };
        if request.since() > version {
            return Err(Fail::invalid_method(
                object,
                "request is newer than the bound version",
            ));
        }
        let destructor = request.is_destructor();
        self.handle(object, version, request)?;
        if destructor {
            self.release_id(object);
        }
        Ok(())
    }

    fn handle(&mut self, object: ObjectId, version: u32, request: AnyRequest) -> Result<(), Fail> {
        match request {
            AnyRequest::WlDisplay(request) => self.display(request),
            AnyRequest::WlRegistry(request) => self.registry(object, request),
            AnyRequest::WlCompositor(request) => self.compositor(version, request),
            AnyRequest::WlShm(request) => self.shm(version, request),
            AnyRequest::WlShmPool(request) => self.pool(object, request),
            AnyRequest::WlOutput(_) => Ok(()),
            AnyRequest::WlSurface(request) => self.surface(object, version, request),
            AnyRequest::XdgWmBase(request) => self.xdg_wm_base(object, version, request),
            AnyRequest::XdgSurface(request) => self.xdg_surface(object, version, request),
            AnyRequest::XdgToplevel(request) => self.xdg_toplevel(object, request),
            AnyRequest::XdgPositioner(_) => Ok(()),
            AnyRequest::WlSeat(request) => self.seat(object, version, request),
            AnyRequest::WlPointer(request) => self.pointer(object, request),
            AnyRequest::WlKeyboard(_) => Ok(()),
            AnyRequest::ZwlrLayerShellV1(request) => self.layer_shell(object, version, request),
            AnyRequest::ZwlrLayerSurfaceV1(request) => self.layer_surface(object, request),
            AnyRequest::ZwlrForeignToplevelManagerV1(request) => {
                self.foreign_manager(object, request)
            }
            AnyRequest::ZwlrForeignToplevelHandleV1(request) => {
                self.foreign_handle(object, request)
            }
            AnyRequest::WpSecurityContextManagerV1(request) => {
                self.security_manager(object, request)
            }
            AnyRequest::WpSecurityContextV1(request) => self.security_context(object, request),
            // 其余对象（buffer、region、callback）只有 destroy 或没有请求，销毁由通用路径完成。
            AnyRequest::WlBuffer(wl_buffer::Request::Destroy) | AnyRequest::WlRegion(_) => Ok(()),
            other => Err(Fail::invalid_method(
                object,
                format!("unsupported request {other:?}"),
            )),
        }
    }

    fn display(&mut self, request: wl_display::Request) -> Result<(), Fail> {
        match request {
            wl_display::Request::Sync { callback } => {
                self.create(callback, Interface::WlCallback, 1, Data::None)?;
                wl_callback::Event::Done { callback_data: 0 }.encode(callback, &mut self.writer)?;
                self.release_id(callback);
            }
            wl_display::Request::GetRegistry { registry } => {
                self.create(registry, Interface::WlRegistry, 1, Data::None)?;
                for (name, interface, version) in self.globals() {
                    wl_registry::Event::Global {
                        name,
                        interface: interface.name().to_owned(),
                        version,
                    }
                    .encode(registry, &mut self.writer)?;
                }
            }
        }
        Ok(())
    }

    /// 本连接能看到的全局对象。
    fn globals(&self) -> Vec<(u32, Interface, u32)> {
        let privileged = (self.privilege != Privilege::Application)
            .then_some(PRIVILEGED_GLOBALS)
            .into_iter()
            .flatten();
        let session = (self.privilege == Privilege::Session).then_some((
            8,
            Interface::WpSecurityContextManagerV1,
            1,
        ));
        GLOBALS
            .into_iter()
            .chain(privileged)
            .chain(session)
            .collect()
    }

    fn registry(&mut self, registry: ObjectId, request: wl_registry::Request) -> Result<(), Fail> {
        let wl_registry::Request::Bind { name, id } = request;
        let global = self
            .globals()
            .into_iter()
            .find(|(candidate, ..)| *candidate == name);
        let Some((_, interface, maximum)) = global else {
            return Err(Fail::invalid_object(format!("no global with name {name}")));
        };
        if id.interface != interface.name() || id.version == 0 || id.version > maximum {
            return Err(Fail::invalid_object(format!(
                "cannot bind {} at version {}",
                id.interface, id.version
            )));
        }
        self.create(id.id, interface, id.version, Data::None)?;
        if interface == Interface::WlShm {
            for format in [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888] {
                wl_shm::Event::Format { format }.encode(id.id, &mut self.writer)?;
            }
        }
        if interface == Interface::WlOutput {
            self.announce_output(id.id, id.version)?;
        }
        if interface == Interface::WlSeat {
            self.announce_seat(id.id, id.version)?;
        }
        if interface == Interface::ZwlrForeignToplevelManagerV1 {
            self.bind_foreign_manager(id.id, id.version)?;
        }
        let _ = registry;
        Ok(())
    }

    fn compositor(&mut self, version: u32, request: wl_compositor::Request) -> Result<(), Fail> {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                self.create(
                    id,
                    Interface::WlSurface,
                    version,
                    Data::Surface(Surface::new()),
                )?;
                // 版本 6 起由服务端告知首选 buffer scale；客户端据此直接按输出 scale 渲染。
                if version >= 6 {
                    wl_surface::Event::PreferredBufferScale {
                        factor: self.output.scale,
                    }
                    .encode(id, &mut self.writer)?;
                }
                Ok(())
            }
            wl_compositor::Request::CreateRegion { id } => {
                self.create(id, Interface::WlRegion, 1, Data::None)
            }
        }
    }

    fn shm(&mut self, version: u32, request: wl_shm::Request) -> Result<(), Fail> {
        match request {
            wl_shm::Request::CreatePool { id, fd, size } => {
                let size = usize::try_from(size)
                    .ok()
                    .filter(|size| *size > 0)
                    .ok_or_else(|| {
                        Fail::new(
                            id,
                            wl_shm::Error::InvalidStride as u32,
                            "pool size must be positive",
                        )
                    })?;
                self.create(
                    id,
                    Interface::WlShmPool,
                    version,
                    Data::Pool(Pool::new(fd, size)),
                )
            }
            wl_shm::Request::Release => Ok(()),
        }
    }

    fn pool(&mut self, object: ObjectId, request: wl_shm_pool::Request) -> Result<(), Fail> {
        let Some(Entry {
            data: Data::Pool(pool),
            version,
            ..
        }) = self.objects.get(&object.get())
        else {
            return Err(Fail::implementation(object));
        };
        let (pool, version) = (Rc::clone(pool), *version);
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let invalid = |message: &str| {
                    Fail::new(
                        object,
                        wl_shm::Error::InvalidStride as u32,
                        message.to_owned(),
                    )
                };
                if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
                    return Err(Fail::new(
                        object,
                        wl_shm::Error::InvalidFormat as u32,
                        "unsupported pixel format",
                    ));
                }
                let (offset, width, height, stride) = (
                    usize::try_from(offset).map_err(|_| invalid("negative offset"))?,
                    usize::try_from(width)
                        .ok()
                        .filter(|v| *v > 0)
                        .ok_or_else(|| invalid("width must be positive"))?,
                    usize::try_from(height)
                        .ok()
                        .filter(|v| *v > 0)
                        .ok_or_else(|| invalid("height must be positive"))?,
                    usize::try_from(stride).map_err(|_| invalid("negative stride"))?,
                );
                let last_row = stride
                    .checked_mul(height - 1)
                    .and_then(|bytes| bytes.checked_add(offset));
                let end = last_row.and_then(|bytes| bytes.checked_add(width * 4));
                if stride < width * 4 || end.is_none_or(|end| end > pool.size()) {
                    return Err(invalid("buffer does not fit the pool"));
                }
                let spec = BufferSpec {
                    pool,
                    offset,
                    width,
                    height,
                    stride,
                    format,
                };
                self.create(id, Interface::WlBuffer, version, Data::Buffer(spec))
            }
            wl_shm_pool::Request::Resize { size } => {
                match usize::try_from(size) {
                    Ok(size) if size >= pool.size() => pool.grow(size),
                    _ => {
                        return Err(Fail::new(
                            object,
                            wl_shm::Error::InvalidFd as u32,
                            "pools can only grow",
                        ));
                    }
                }
                Ok(())
            }
            // Pool 对象销毁后 buffer 仍持有 Rc，内存由最后一个引用释放。
            wl_shm_pool::Request::Destroy => Ok(()),
        }
    }

    pub(super) fn surface_mut(&mut self, object: ObjectId) -> Result<&mut Surface, Fail> {
        match self.objects.get_mut(&object.get()) {
            Some(Entry {
                data: Data::Surface(surface),
                ..
            }) => Ok(surface),
            _ => Err(Fail::implementation(object)),
        }
    }

    fn surface(
        &mut self,
        object: ObjectId,
        version: u32,
        request: wl_surface::Request,
    ) -> Result<(), Fail> {
        use wl_surface::Request as R;
        match request {
            R::Destroy => {
                // 未呈现的 frame callback 随 surface 一起回收，否则客户端的 id 永远无法复用。
                let surface = self.surface_mut(object)?;
                let orphans: Vec<ObjectId> = surface
                    .pending
                    .frame_callbacks
                    .drain(..)
                    .chain(surface.queued_callbacks.drain(..))
                    .collect();
                for callback in orphans {
                    self.release_id(callback);
                }
                Ok(())
            }
            R::Attach { buffer, x, y } => {
                if version >= 5 && (x != 0 || y != 0) {
                    return Err(Fail::new(
                        object,
                        wl_surface::Error::InvalidOffset as u32,
                        "attach offset must be zero",
                    ));
                }
                if let Some(buffer) = buffer {
                    let is_buffer = matches!(
                        self.objects.get(&buffer.get()),
                        Some(Entry {
                            data: Data::Buffer(_),
                            ..
                        })
                    );
                    if !is_buffer {
                        return Err(Fail::invalid_object(format!(
                            "object {} is not a wl_buffer",
                            buffer.get()
                        )));
                    }
                }
                self.surface_mut(object)?.pending.buffer = Some(buffer);
                Ok(())
            }
            R::Frame { callback } => {
                self.create(callback, Interface::WlCallback, 1, Data::None)?;
                self.surface_mut(object)?
                    .pending
                    .frame_callbacks
                    .push(callback);
                Ok(())
            }
            R::SetBufferScale { scale } => {
                if scale < 1 {
                    return Err(Fail::new(
                        object,
                        wl_surface::Error::InvalidScale as u32,
                        "buffer scale must be positive",
                    ));
                }
                self.surface_mut(object)?.pending.scale = Some(scale);
                Ok(())
            }
            R::SetBufferTransform { transform } => {
                if !(0..=7).contains(&transform) {
                    return Err(Fail::new(
                        object,
                        wl_surface::Error::InvalidTransform as u32,
                        "invalid buffer transform",
                    ));
                }
                Ok(())
            }
            R::Commit => self.commit(object),
            // 损伤区域只用于局部重绘；本合成器每次整屏重画，所以这些请求无需记录。
            R::Damage { .. }
            | R::DamageBuffer { .. }
            | R::SetOpaqueRegion { .. }
            | R::SetInputRegion { .. }
            | R::Offset { .. } => Ok(()),
        }
    }

    fn commit(&mut self, object: ObjectId) -> Result<(), Fail> {
        self.xdg_check_commit(object)?;
        let layer_before = self.layer_state(object);
        self.layer_check_commit(object)?;
        self.commits += 1;
        let surface = self.surface_mut(object)?;
        let choice = surface.pending.buffer.take();
        let callbacks = std::mem::take(&mut surface.pending.frame_callbacks);
        if let Some(scale) = surface.pending.scale.take() {
            surface.scale = scale;
        }
        surface.queued_callbacks.extend(callbacks);
        match choice {
            None => {}
            Some(None) => self.surface_mut(object)?.content = None,
            Some(Some(buffer)) => {
                let Some(Entry {
                    data: Data::Buffer(spec),
                    ..
                }) = self.objects.get(&buffer.get())
                else {
                    return Err(Fail::invalid_object(format!(
                        "buffer {} was destroyed before commit",
                        buffer.get()
                    )));
                };
                let pixels = spec.read_pixels().map_err(|_| {
                    Fail::new(
                        buffer,
                        wl_shm::Error::InvalidFd as u32,
                        "buffer extends past the end of its file",
                    )
                })?;
                let content = Content {
                    width: spec.width,
                    height: spec.height,
                    pixels,
                };
                let first_content = {
                    let surface = self.surface_mut(object)?;
                    surface.content = Some(content);
                    !std::mem::replace(&mut surface.entered, true)
                };
                if first_content {
                    self.enter_outputs(object)?;
                }
                // 内容已复制，buffer 立即可复用。
                wl_buffer::Event::Release.encode(buffer, &mut self.writer)?;
            }
        }
        if choice == Some(None) {
            self.layer_unmap(object);
        } else {
            self.layer_after_commit(object, layer_before)?;
        }
        self.xdg_after_commit(object)
    }
}

impl Client {
    /// `wl_output` 绑定后依次宣告几何、模式、scale、名字与 `done`；字段按绑定版本取舍。
    fn announce_output(&mut self, output: ObjectId, version: u32) -> Result<(), Fail> {
        let Output {
            width,
            height,
            scale,
        } = self.output;
        let events = &mut self.writer;
        wl_output::Event::Geometry {
            x: 0,
            y: 0,
            physical_width: 0,
            physical_height: 0,
            subpixel: wl_output::Subpixel::Unknown as i32,
            make: "LiteOS".into(),
            model: "Virtual Display".into(),
            transform: wl_output::Transform::Normal as i32,
        }
        .encode(output, events)?;
        wl_output::Event::Mode {
            flags: wl_output::mode::CURRENT | wl_output::mode::PREFERRED,
            width,
            height,
            refresh: 60_000,
        }
        .encode(output, events)?;
        if version >= 2 {
            wl_output::Event::Scale { factor: scale }.encode(output, events)?;
        }
        if version >= 4 {
            wl_output::Event::Name {
                name: "LITEOS-1".into(),
            }
            .encode(output, events)?;
            wl_output::Event::Description {
                description: "LiteOS virtual display".into(),
            }
            .encode(output, events)?;
        }
        if version >= 2 {
            wl_output::Event::Done.encode(output, events)?;
        }
        Ok(())
    }

    /// surface 第一次有内容时告诉客户端它位于哪些输出上；缺少它，客户端不知道自己的有效 scale。
    fn enter_outputs(&mut self, surface: ObjectId) -> Result<(), Fail> {
        let outputs: Vec<ObjectId> = self
            .objects
            .iter()
            .filter(|(_, entry)| entry.interface == Interface::WlOutput)
            .filter_map(|(id, _)| ObjectId::new(*id))
            .collect();
        for output in outputs {
            wl_surface::Event::Enter { output }.encode(surface, &mut self.writer)?;
        }
        Ok(())
    }
}
