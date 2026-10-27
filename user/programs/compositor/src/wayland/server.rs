//! 监听 socket 与连接：把字节和描述符搬进 [`Client`]，把输出搬回 socket。

use std::{
    fs,
    io::{self, Write},
    os::{
        fd::AsFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
};

use linux_uapi::unix::{PollEvents, PollFd, recv_fds, send_fd};
use wayland_wire::wire::ObjectId;

use super::{
    Client, Content, CursorRequest, ForeignInfo, ForeignRequest, LayerView, Output, Window,
    WindowRequest,
};

/// 每次读取的上限；一条消息至多 4096 字节，所以一次读取至少能推进状态机。
const READ_BYTES: usize = 4096;

struct Connection {
    id: ClientId,
    stream: UnixStream,
    client: Client,
    /// 协议错误或对端关闭：发完剩余输出后移除。
    closing: bool,
}

impl Connection {
    /// 把待发输出尽量写入 socket；返回是否已全部发完。带描述符的段用 `SCM_RIGHTS` 一并发送。
    fn flush(&mut self) -> io::Result<bool> {
        self.client.flush_buffer_releases();
        loop {
            let (bytes, fd) = self.client.output();
            if bytes.is_empty() {
                return Ok(true);
            }
            let result = match fd {
                Some(fd) => send_fd(self.stream.as_fd(), bytes, fd),
                None => self.stream.write(bytes),
            };
            match result {
                Ok(0) => {
                    self.closing = true;
                    self.client.discard_output();
                    return Err(io::ErrorKind::WriteZero.into());
                }
                Ok(written) => self.client.consume_output(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.closing = true;
                    self.client.discard_output();
                    return Err(error);
                }
            }
        }
    }

    /// 读空 socket 并处理消息；对端关闭、读错误或协议错误都会标记 `closing`。
    fn read(&mut self) {
        let mut buffer = [0u8; READ_BYTES];
        loop {
            match recv_fds(self.stream.as_fd(), &mut buffer) {
                Ok((0, _)) => {
                    self.closing = true;
                    return;
                }
                Ok((count, fds)) => {
                    if self.client.receive(&buffer[..count], fds).is_err() {
                        self.closing = true;
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.closing = true;
                    return;
                }
            }
        }
    }
}

/// 稳定的连接标识：连接在列表中的序号会随断开而移位，持久状态（窗口栈）必须用它做键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

/// 监听 socket 与全部连接。
pub struct Server {
    listener: UnixListener,
    connections: Vec<Connection>,
    next_id: u64,
    output: Output,
    contexts: Vec<super::security::Listener>,
    /// 当前剪贴板选择：持有者连接、它的 `wl_data_source` 与声明的类型。
    selection: Option<Selection>,
    gpu: Option<linux_uapi::drm::VirglContext>,
}

struct Selection {
    owner: ClientId,
    source: ObjectId,
    mimes: Vec<String>,
}

impl Server {
    /// 在 `path` 创建监听 socket：清理残留的同名 socket，权限 0666，非阻塞。
    ///
    /// 任何本地用户都能连接；本监听入口始终只授予普通权限，外壳特权来自独立 socketpair。
    ///
    /// # Errors
    ///
    /// 目录不存在、无法绑定或设置权限。
    pub fn bind(path: &Path, output: Output) -> io::Result<Self> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            connections: Vec::new(),
            next_id: 1,
            output,
            contexts: Vec::new(),
            selection: None,
            gpu: None,
        })
    }

    /// 连接建立前注入合成器已初始化的唯一 GPU context；CPU 后端不发布 linux-dmabuf。
    pub fn set_gpu_context(&mut self, context: Option<linux_uapi::drm::VirglContext>) {
        self.gpu = context;
    }

    #[cfg(test)]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    #[cfg(test)]
    pub(super) fn context_count(&self) -> usize {
        self.contexts.len()
    }

    /// poll 描述符：先是监听 socket，随后每个连接一个；有未发完输出的连接额外关注可写。
    pub fn descriptors(&self) -> impl Iterator<Item = PollFd<'_>> {
        std::iter::once(PollFd::new(self.listener.as_fd(), PollEvents::READ))
            .chain(self.connections.iter().map(|connection| {
                let events = if connection.client.output().0.is_empty() {
                    PollEvents::READ
                } else {
                    PollEvents::READ | PollEvents::WRITE
                };
                PollFd::new(connection.stream.as_fd(), events)
            }))
            .chain(
                self.contexts
                    .iter()
                    .flat_map(|context| context.descriptors()),
            )
    }

    /// 处理一次 poll 的结果：`returned` 与 [`Server::descriptors`] 的顺序和数量一致。
    ///
    /// # Errors
    ///
    /// 监听 socket 的 accept 失败（单个连接的错误只会关闭该连接）。
    pub fn dispatch(&mut self, returned: &[PollEvents]) -> io::Result<()> {
        let context_start = self.connections.len() + 1;
        debug_assert_eq!(returned.len(), context_start + self.contexts.len() * 2);
        // 先处理本轮快照中的 listener，再接收新注册项；同轮 close 的 listener 优先于 accept，避免撤销后仍接纳连接。
        let mut accepted = Vec::new();
        let mut index = 0;
        self.contexts.retain(|context| {
            let events = &returned[context_start + index * 2..context_start + index * 2 + 2];
            index += 1;
            if super::security::Listener::revoked(events) {
                println!("compositor: context listener revoked");
                return false;
            }
            if events[0].contains(PollEvents::READ) {
                loop {
                    match context.listener.accept() {
                        Ok((stream, _)) => accepted.push(stream),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => return false,
                    }
                }
            }
            true
        });
        let accept = returned
            .first()
            .is_some_and(|events| events.contains(PollEvents::READ));
        for (connection, events) in self.connections.iter_mut().zip(&returned[1..context_start]) {
            if events.contains(PollEvents::READ)
                || events.contains(PollEvents::HANGUP)
                || events.contains(PollEvents::ERROR)
            {
                connection.read();
            }
            self.contexts
                .extend(connection.client.take_security_contexts());
            if connection.flush().is_err() {
                connection.closing = true;
            }
        }
        for stream in accepted {
            self.add_connection(stream, Client::shell(self.output))?;
        }
        // 先发完再移除：协议错误事件必须在断开前到达客户端。
        self.connections
            .retain(|connection| !(connection.closing && connection.client.output().0.is_empty()));
        if accept {
            self.accept()?;
        }
        Ok(())
    }

    /// 加入 compositor 自己创建 socketpair 的初始 Session（logon）；委派后的桌面只能经 context accept 取得 Shell 能力。
    ///
    /// # Errors
    ///
    /// 无法把 socket 设为非阻塞。
    pub fn add_session(&mut self, stream: UnixStream) -> io::Result<ClientId> {
        self.add_connection(stream, Client::session(self.output))
    }

    fn add_connection(&mut self, stream: UnixStream, mut client: Client) -> io::Result<ClientId> {
        stream.set_nonblocking(true)?;
        client.gpu = self.gpu.clone();
        let id = ClientId(self.next_id);
        self.next_id += 1;
        self.connections.push(Connection {
            id,
            stream,
            client,
            closing: false,
        });
        Ok(id)
    }

    fn accept(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    self.add_connection(stream, Client::new(self.output))?;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// 本帧已呈现：通知所有连接并尽量发出。
    pub fn finish_frame(&mut self, time_ms: u32) {
        for connection in &mut self.connections {
            connection.client.finish_frame(time_ms);
            if connection.flush().is_err() {
                connection.closing = true;
            }
        }
    }

    /// 全部连接的已映射顶层窗口。
    pub fn windows(&self) -> impl Iterator<Item = (ClientId, Window<'_>)> {
        self.connections.iter().flat_map(|connection| {
            connection
                .client
                .windows()
                .map(move |window| (connection.id, window))
        })
    }

    /// 全部连接的已映射 layer surface（桌面外壳）。
    pub fn layers(&self) -> impl Iterator<Item = (ClientId, LayerView<'_>)> {
        self.connections.iter().flat_map(|connection| {
            connection
                .client
                .layers()
                .map(move |layer| (connection.id, layer))
        })
    }

    /// 全部连接已发生的 surface commit 总数；变化意味着画面内容可能变了。
    pub fn commits(&self) -> u64 {
        self.connections
            .iter()
            .map(|connection| connection.client.commits())
            .sum()
    }

    /// 取走全部连接自上次以来提出的窗口请求。
    pub fn take_requests(&mut self) -> Vec<(ClientId, ObjectId, WindowRequest)> {
        let mut all = Vec::new();
        for connection in &mut self.connections {
            let id = connection.id;
            all.extend(
                connection
                    .client
                    .take_requests()
                    .into_iter()
                    .map(|(surface, request)| (id, surface, request)),
            );
        }
        all
    }

    /// 对连接 `id` 的 `Client` 执行动作（发送 configure/close）并立刻尝试发出输出；连接已断开则忽略。
    pub fn with_client(&mut self, id: ClientId, action: impl FnOnce(&mut Client)) {
        if let Some(connection) = self
            .connections
            .iter_mut()
            .find(|connection| connection.id == id)
        {
            action(&mut connection.client);
            if connection.flush().is_err() {
                connection.closing = true;
            }
        }
    }

    /// 让所有绑定了窗口管理器的外壳连接看到与 `windows` 一致的列表。
    pub fn sync_foreign(&mut self, windows: &[ForeignInfo]) {
        for connection in &mut self.connections {
            if connection.client.has_foreign_manager() {
                connection.client.sync_foreign(windows);
                if connection.flush().is_err() {
                    connection.closing = true;
                }
            }
        }
    }

    /// 取走外壳提出的窗口操作。
    pub fn take_foreign_requests(&mut self) -> Vec<ForeignRequest> {
        self.connections
            .iter_mut()
            .flat_map(|connection| {
                let shell = connection.id;
                connection
                    .client
                    .take_foreign_requests()
                    .into_iter()
                    .map(move |request| ForeignRequest { shell, ..request })
            })
            .collect()
    }

    /// 处理剪贴板：采纳键盘焦点客户端的 `set_selection`，把读取请求转交持有者，并在持有者断开时清空选择。
    ///
    /// 只有键盘焦点所在的客户端能设置选择；缺少这个限制，后台程序能随时覆盖用户刚复制的内容。
    pub fn handle_selection(&mut self, focus: Option<ClientId>) {
        let mut changed = false;
        let mut sets = Vec::new();
        let mut receives = Vec::new();
        for connection in &mut self.connections {
            if let Some(set) = connection.client.selection_set.take() {
                sets.push((connection.id, set));
            }
            receives.append(&mut connection.client.receives);
        }
        for (client, set) in sets {
            if Some(client) != focus {
                continue;
            }
            let old = self.selection.take();
            let next = set.source.map(|source| Selection {
                owner: client,
                source,
                mimes: set.mimes,
            });
            // 旧来源被替换或清空：告诉它。同一个来源重复设置不算替换。
            if let Some(old) = old
                && next
                    .as_ref()
                    .is_none_or(|n| (n.owner, n.source) != (old.owner, old.source))
            {
                self.with_client(old.owner, |client| client.source_cancelled(old.source));
            }
            self.selection = next;
            changed = true;
        }
        for request in receives {
            if let Some(selection) = &self.selection {
                let (owner, source) = (selection.owner, selection.source);
                self.with_client(owner, |client| {
                    client.source_send(source, request.mime, request.fd)
                });
            }
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|s| !self.connections.iter().any(|c| c.id == s.owner))
        {
            self.selection = None;
            changed = true;
        }
        if changed {
            self.announce_selection(focus);
        }
    }

    /// 把当前选择（或空）告诉 `client`；键盘焦点换到新客户端、选择变化时调用。
    pub fn announce_selection(&mut self, client: Option<ClientId>) {
        let Some(client) = client else { return };
        let mimes = self.selection.as_ref().map(|s| s.mimes.clone());
        self.with_client(client, |client| {
            let _ = client.send_selection(mimes.as_deref());
        });
    }

    /// 全部连接的弹出层，附带所属连接；位置相对各自根窗口 surface。
    pub fn popups(&self) -> Vec<(ClientId, super::popup::PopupView)> {
        self.connections
            .iter()
            .flat_map(|connection| {
                connection
                    .client
                    .popups()
                    .into_iter()
                    .map(move |view| (connection.id, view))
            })
            .collect()
    }

    /// 点击了弹出层之外：让 `client` 的 grab 弹出层结束。
    pub fn dismiss_grabbed_popups(&mut self, client: ClientId) {
        self.with_client(client, |client| client.dismiss_grabbed_popups());
    }

    /// 取走全部连接的光标请求。
    pub fn take_cursor_requests(&mut self) -> Vec<(ClientId, CursorRequest)> {
        let mut all = Vec::new();
        for connection in &mut self.connections {
            let id = connection.id;
            all.extend(
                connection
                    .client
                    .take_cursor_requests()
                    .into_iter()
                    .map(|request| (id, request)),
            );
        }
        all
    }

    /// 已提交 surface 树的可见像素引用；只有合成与命中需要遍历，窗口元数据查询不分配子树。
    pub fn subtree(&self, client: ClientId, surface: ObjectId) -> Vec<super::ChildView<'_>> {
        self.connections
            .iter()
            .find(|connection| connection.id == client)
            .map_or_else(Vec::new, |connection| connection.client.subtree(surface))
    }

    /// 指针命中使用当前提交的 input region，不能将点击交给只是矩形重叠的窗口。
    pub fn accepts_input(&self, client: ClientId, surface: ObjectId, x: f64, y: f64) -> bool {
        self.connections
            .iter()
            .find(|connection| connection.id == client)
            .is_some_and(|connection| connection.client.accepts_input(surface, x, y))
    }

    pub fn root_surface(&self, client: ClientId, surface: ObjectId) -> Option<ObjectId> {
        self.connections
            .iter()
            .find(|connection| connection.id == client)?
            .client
            .root_surface(surface)
    }

    /// 本次提交包含的 surface 回调进入在途集合，之后才提交的 callback 留待下一次显示。
    pub fn stage_frame(&mut self, surfaces: &[(ClientId, ObjectId)]) {
        for (owner, surface) in surfaces {
            self.with_client(*owner, |client| client.stage_surface_frame(*surface));
        }
    }

    /// 某个连接的光标 surface 的当前已提交图像与采样几何。
    pub fn cursor_image(&self, client: ClientId, surface: ObjectId) -> Option<&Content> {
        self.connections
            .iter()
            .find(|connection| connection.id == client)?
            .client
            .cursor_image(surface)
    }
}
