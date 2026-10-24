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
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => self.client.consume_output(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
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
        })
    }

    #[cfg(test)]
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// poll 描述符：先是监听 socket，随后每个连接一个；有未发完输出的连接额外关注可写。
    pub fn descriptors(&self) -> Vec<PollFd> {
        let mut descriptors = vec![PollFd::new(self.listener.as_fd(), PollEvents::READ)];
        for connection in &self.connections {
            let wants_write = !connection.client.output().0.is_empty();
            let events = if wants_write {
                PollEvents::READ | PollEvents::WRITE
            } else {
                PollEvents::READ
            };
            descriptors.push(PollFd::new(connection.stream.as_fd(), events));
        }
        descriptors
    }

    /// 处理一次 poll 的结果：`returned` 与 [`Server::descriptors`] 的顺序和数量一致。
    ///
    /// # Errors
    ///
    /// 监听 socket 的 accept 失败（单个连接的错误只会关闭该连接）。
    pub fn dispatch(&mut self, returned: &[PollEvents]) -> io::Result<()> {
        debug_assert_eq!(returned.len(), self.connections.len() + 1);
        let accept = returned
            .first()
            .is_some_and(|events| events.contains(PollEvents::READ));
        for (connection, events) in self.connections.iter_mut().zip(&returned[1..]) {
            if events.contains(PollEvents::READ)
                || events.contains(PollEvents::HANGUP)
                || events.contains(PollEvents::ERROR)
            {
                connection.read();
            }
            if connection.flush().is_err() {
                connection.closing = true;
            }
        }
        // 先发完再移除：协议错误事件必须在断开前到达客户端。
        self.connections
            .retain(|connection| !(connection.closing && connection.client.output().0.is_empty()));
        if accept {
            self.accept()?;
        }
        Ok(())
    }

    /// 加入一个已连接的特权客户端（compositor 自己拉起的外壳程序，经 `WAYLAND_SOCKET` 传给它）。
    ///
    /// # Errors
    ///
    /// 无法把 socket 设为非阻塞。
    pub fn add_privileged(&mut self, stream: UnixStream) -> io::Result<ClientId> {
        stream.set_nonblocking(true)?;
        let id = ClientId(self.next_id);
        self.next_id += 1;
        self.connections.push(Connection {
            id,
            stream,
            client: Client::privileged(self.output),
            closing: false,
        });
        Ok(id)
    }

    fn accept(&mut self) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    let id = ClientId(self.next_id);
                    self.next_id += 1;
                    self.connections.push(Connection {
                        id,
                        stream,
                        client: Client::new(self.output),
                        closing: false,
                    });
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
            .flat_map(|connection| connection.client.take_foreign_requests())
            .collect()
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

    /// 某个连接的光标 surface 的当前图像与 buffer scale。
    pub fn cursor_image(&self, client: ClientId, surface: ObjectId) -> Option<(&Content, i32)> {
        self.connections
            .iter()
            .find(|connection| connection.id == client)?
            .client
            .cursor_image(surface)
    }
}
