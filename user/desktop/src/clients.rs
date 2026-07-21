//! 显示客户端会话：连接集、握手与 surface 生命周期的协议消息处理。
//!
//! 协议帧假设一次 `recv_message` 返回整数条完整帧（发送方单条 write、
//! 帧 ≤ [`MAX_MESSAGE`] 字节，unix stream 本地传输下成立）；读到截断帧视为
//! 协议错误并断开该客户端。客户端不会向桌面发 fd，握手外收到的 fd 立即
//! close 防泄漏。

use display_proto::{
    self, Commit, CreateSurface, DestroySurface, Hello, SetBuffer, SetTitle, SurfaceCreated,
    Welcome,
};
use linux_uapi::drm::GemHandle;
use std::{io::Write, os::unix::net::UnixStream};

use crate::{
    chrome,
    compositor::Damage,
    pointer,
    scanout::{Rect, Scanout},
    taskbar::Taskbar,
    window::{SurfaceDesc, Windows},
};

/// client 在 `Clients` 中的条目。
struct Client {
    stream: UnixStream,
    /// 是否已完成 HELLO → WELCOME 握手。
    greeted: bool,
}

/// 客户端连接集；每个 socket 由本结构唯一持有。
pub struct Clients {
    list: Vec<Option<Client>>,
}

impl Clients {
    pub fn new() -> Self {
        Self { list: Vec::new() }
    }

    pub fn add(&mut self, stream: UnixStream) -> Option<usize> {
        self.list.try_reserve(1).ok()?;
        let slot = self
            .list
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.list.len());
        let client = Client {
            stream,
            greeted: false,
        };
        if slot == self.list.len() {
            self.list.push(Some(client));
        } else {
            self.list[slot] = Some(client);
        }
        Some(slot)
    }

    /// 遍历连接槽位（事件循环构造 poll 数组用）；返回 `(槽位, fd)`。
    pub fn slots(&self) -> impl Iterator<Item = (usize, &UnixStream)> {
        self.list
            .iter()
            .enumerate()
            .filter_map(|(index, client)| client.as_ref().map(|client| (index, &client.stream)))
    }

    pub fn active_len(&self) -> usize {
        self.list.iter().filter(|client| client.is_some()).count()
    }

    /// 发送一条已编码帧；失败（对端异常 / 缓冲满）静默忽略——对端死亡会
    /// 经 EOF / POLLERR 路径回收。
    pub fn send(&self, index: usize, buffer: &[u8]) {
        let Some(client) = self.list.get(index).and_then(Option::as_ref) else {
            return;
        };
        let _ = display_proto::send_message(&client.stream, buffer);
    }

    fn remove(&mut self, index: usize) {
        self.list[index] = None;
    }

    fn client(&self, index: usize) -> Option<&Client> {
        self.list.get(index)?.as_ref()
    }

    fn client_mut(&mut self, index: usize) -> Option<&mut Client> {
        self.list.get_mut(index)?.as_mut()
    }

    fn greeted(&self, index: usize) -> bool {
        self.client(index).is_some_and(|client| client.greeted)
    }
}

/// 每条只打一次的 stderr marker（进 UART 日志，runtime gate 匹配）。
pub struct Markers {
    client: bool,
    surface: bool,
}

impl Markers {
    pub fn new() -> Self {
        Self {
            client: false,
            surface: false,
        }
    }
}

/// 事件处理共享的可变状态束（避免函数签名参数过长）。
pub struct Shell<'a> {
    pub clients: &'a mut Clients,
    pub windows: &'a mut Windows,
    pub damage: &'a mut Damage,
    pub scanout: &'a Scanout,
    pub taskbar: &'a mut Taskbar,
    pub markers: &'a mut Markers,
}

/// 消费一个 client socket 上的所有待读消息；返回 `false` 表示连接应回收
/// （EOF / 协议错误 / 系统错误）。
pub fn service_client(index: usize, shell: &mut Shell) -> bool {
    let mut buffer = [0u8; display_proto::MAX_MESSAGE];
    loop {
        let Some(client) = shell.clients.client(index) else {
            return false;
        };
        let received = display_proto::recv_message(&client.stream, &mut buffer);
        match received {
            Ok((0, _)) => return false,
            Ok((_, Some(_))) => return false,
            Ok((length, None)) => {
                let mut offset = 0;
                while offset < length {
                    let Some((header, payload)) =
                        display_proto::parse_header(&buffer[offset..length])
                    else {
                        return false;
                    };
                    offset += header.len as usize;
                    if !handle_message(index, header.kind, payload, shell) {
                        return false;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

fn handle_message(index: usize, kind: u32, payload: &[u8], shell: &mut Shell) -> bool {
    match kind {
        display_proto::HELLO => {
            let Some(hello) = Hello::parse(payload) else {
                return false;
            };
            if shell.clients.greeted(index) || hello.version != display_proto::PROTOCOL_VERSION {
                return false;
            }
            let welcome = Welcome {
                version: display_proto::PROTOCOL_VERSION,
            };
            let mut frame = [0u8; 16];
            let Some(length) = welcome.encode(&mut frame) else {
                return false;
            };
            let Some(client) = shell.clients.client(index) else {
                return false;
            };
            if display_proto::send_message_with_fd(
                &client.stream,
                &frame[..length],
                shell.scanout.drm_device().as_fd(),
            )
            .is_err()
            {
                return false;
            }
            let Some(client) = shell.clients.client_mut(index) else {
                return false;
            };
            client.greeted = true;
            if !shell.markers.client {
                mark(b"desktop: client connected\n");
                shell.markers.client = true;
            }
            true
        }
        display_proto::CREATE_SURFACE if shell.clients.greeted(index) => {
            let Some((create, title)) = CreateSurface::parse(payload) else {
                return false;
            };
            create_surface(index, &create, title, shell);
            true
        }
        display_proto::COMMIT if shell.clients.greeted(index) => {
            let Some((commit, rects)) = Commit::parse(payload) else {
                return false;
            };
            let Some(slot) = shell.windows.by_surface(commit.surface_id) else {
                return true;
            };
            let Some(window) = shell.windows.get(slot) else {
                return true;
            };
            if window.client != index {
                return true;
            }
            let content = window.content_rect();
            if commit.num_rects == 0 {
                shell.damage.add(content);
            } else {
                for [x1, y1, x2, y2] in rects {
                    shell.damage.add(
                        Rect::new(
                            content.x1 + i32::from(x1),
                            content.y1 + i32::from(y1),
                            content.x1 + i32::from(x2),
                            content.y1 + i32::from(y2),
                        )
                        .intersect(content),
                    );
                }
            }
            true
        }
        display_proto::SET_TITLE if shell.clients.greeted(index) => {
            let Some((set_title, title)) = SetTitle::parse(payload) else {
                return false;
            };
            let Some(slot) = shell.windows.by_surface(set_title.surface_id) else {
                return true;
            };
            let Some(window) = shell.windows.get_mut(slot) else {
                return true;
            };
            if window.client != index {
                return true;
            }
            window.set_title(title);
            let outer = window.outer_rect();
            shell.damage.add(Rect::new(
                outer.x1,
                outer.y1,
                outer.x2,
                outer.y1 + chrome::TITLE_HEIGHT,
            ));
            // 任务栏窗口按钮文字变化：只 damage 该按钮矩形。
            if let Some(rect) = shell
                .taskbar
                .window_button_rect(shell.windows, set_title.surface_id)
            {
                shell.damage.add(rect);
            }
            true
        }
        display_proto::SET_BUFFER if shell.clients.greeted(index) => {
            let Some(set_buffer) = SetBuffer::parse(payload) else {
                return false;
            };
            apply_set_buffer(index, &set_buffer, shell);
            true
        }
        display_proto::DESTROY_SURFACE if shell.clients.greeted(index) => {
            let Some(destroy) = DestroySurface::parse(payload) else {
                return false;
            };
            let Some(slot) = shell.windows.by_surface(destroy.surface_id) else {
                return true;
            };
            if shell
                .windows
                .get(slot)
                .is_some_and(|window| window.client != index)
            {
                return true;
            }
            destroy_window(slot, shell);
            true
        }
        _ => true,
    }
}

/// `CREATE_SURFACE`：映射客户端 handle（所有权随消息转移给桌面）并建窗
/// 置顶聚焦；任何失败都回 `SURFACE_CREATED{error}` 且由桌面销毁 handle。
fn create_surface(index: usize, create: &CreateSurface, title: &[u8], shell: &mut Shell) {
    let reply = |surface_id: u32, error: u32| {
        let message = SurfaceCreated { surface_id, error };
        let mut frame = [0u8; 24];
        if let Some(length) = message.encode(&mut frame) {
            shell.clients.send(index, &frame[..length]);
        }
    };
    const EINVAL: u32 = 22;
    const ENOMEM: u32 = 12;
    let width = create.width as usize;
    let height = create.height as usize;
    let Some(handle) = GemHandle::new(create.gem_handle) else {
        reply(0, EINVAL);
        return;
    };
    if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
        shell.scanout.drm_device().destroy_transferred(handle);
        reply(0, EINVAL);
        return;
    }
    let Ok(buffer) = shell
        .scanout
        .drm_device()
        .adopt_transferred(handle, width, height)
    else {
        reply(0, EINVAL);
        return;
    };
    let added = shell.windows.add(SurfaceDesc {
        client: index,
        buffer,
        decorated: create.flags & display_proto::SURFACE_FLAG_UNDECORATED == 0,
        title,
    });
    let Some((slot, surface_id)) = added else {
        reply(0, ENOMEM);
        return;
    };
    reply(surface_id, 0);
    if !shell.markers.surface {
        mark_surface(surface_id, width, height);
        shell.markers.surface = true;
    }
    if let Some(window) = shell.windows.get(slot) {
        shell.damage.add(window.outer_rect());
    }
    // 新窗口按钮出现在任务栏。
    shell.damage.add(shell.taskbar.strip_rect());
    pointer::set_focus(shell.windows, shell.clients, shell.damage, Some(slot));
}

/// `SET_BUFFER`：客户端响应 `CONFIGURE`（或自行改变内容尺寸）提交的新 backing
/// buffer。校验同 `CREATE_SURFACE`（宽高 1..=4096、handle ≠ 0、surface 属于该
/// client）；新 handle 所有权随消息转移给桌面——无论成败都由桌面销毁旧 handle
/// 语义下的相应 handle：成功时 unmap + DESTROY_DUMB 旧 handle（在
/// [`crate::window::Window::apply_buffer`] 内），失败时销毁新 handle 防泄漏。
/// 窗口内容尺寸以消息的 width/height 为准（锚定左上角）。
fn apply_set_buffer(index: usize, set_buffer: &SetBuffer, shell: &mut Shell) {
    let Some(handle) = GemHandle::new(set_buffer.gem_handle) else {
        return;
    };
    let reject = || {
        shell.scanout.drm_device().destroy_transferred(handle);
    };
    let Some(slot) = shell.windows.by_surface(set_buffer.surface_id) else {
        reject();
        return;
    };
    let Some(window) = shell.windows.get(slot) else {
        reject();
        return;
    };
    if window.client != index {
        reject();
        return;
    }
    let width = set_buffer.width as usize;
    let height = set_buffer.height as usize;
    if !(1..=4096).contains(&width) || !(1..=4096).contains(&height) {
        reject();
        return;
    };
    let Ok(buffer) = shell
        .scanout
        .drm_device()
        .adopt_transferred(handle, width, height)
    else {
        return;
    };
    let old = window.outer_rect();
    let Some(window) = shell.windows.get_mut(slot) else {
        return;
    };
    window.apply_buffer(buffer);
    shell.damage.add(old);
    if let Some(window) = shell.windows.get(slot) {
        shell.damage.add(window.outer_rect());
    }
}

/// 销毁窗口并修补焦点（焦点丢失时回落到栈顶可见窗口）。
fn destroy_window(slot: usize, shell: &mut Shell) {
    if let Some(window) = shell.windows.get(slot) {
        shell.damage.add(window.outer_rect());
    }
    shell.windows.remove(slot);
    shell.damage.add(shell.taskbar.strip_rect());
    if shell.windows.focused().is_none() {
        let top = shell.windows.top_visible();
        pointer::set_focus(shell.windows, shell.clients, shell.damage, top);
    }
}

/// 连接回收：关闭 fd、销毁该 client 的全部窗口（munmap + DESTROY_DUMB +
/// damage 旧区域）、修补焦点。
pub fn drop_client(index: usize, shell: &mut Shell) {
    while let Some(slot) = shell.windows.bottom_to_top().iter().copied().find(|slot| {
        shell
            .windows
            .get(*slot)
            .is_some_and(|window| window.client == index)
    }) {
        if let Some(window) = shell.windows.get(slot) {
            shell.damage.add(window.outer_rect());
        }
        shell.windows.remove(slot);
    }
    shell.damage.add(shell.taskbar.strip_rect());
    if shell.windows.focused().is_none() {
        let top = shell.windows.top_visible();
        pointer::set_focus(shell.windows, shell.clients, shell.damage, top);
    }
    shell.clients.remove(index);
}

fn mark(text: &[u8]) {
    let _ = std::io::stderr().write_all(text);
}

pub fn mark_mode(width: usize, height: usize) {
    let mut line = [0u8; 64];
    let prefix = b"desktop: mode ";
    line[..prefix.len()].copy_from_slice(prefix);
    let mut length = prefix.len();
    length += decimal(width as u32, &mut line[length..]);
    line[length] = b'x';
    length += 1;
    length += decimal(height as u32, &mut line[length..]);
    line[length] = b'\n';
    length += 1;
    mark(&line[..length]);
}

fn mark_surface(surface_id: u32, width: usize, height: usize) {
    let mut line = [0u8; 80];
    let prefix = b"desktop: surface ";
    line[..prefix.len()].copy_from_slice(prefix);
    let mut length = prefix.len();
    length += decimal(surface_id, &mut line[length..]);
    line[length] = b' ';
    length += 1;
    length += decimal(width as u32, &mut line[length..]);
    line[length] = b'x';
    length += 1;
    length += decimal(height as u32, &mut line[length..]);
    line[length] = b'\n';
    length += 1;
    mark(&line[..length]);
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for (index, slot) in output.iter_mut().enumerate().take(count) {
        *slot = digits[count - 1 - index];
    }
    count
}
