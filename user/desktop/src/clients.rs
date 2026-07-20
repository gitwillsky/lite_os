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

use crate::{
    chrome,
    compositor::Damage,
    ffi,
    pointer,
    scanout::{self, Rect, Scanout},
    taskbar::Taskbar,
    window::{SurfaceDesc, Windows},
};

/// 同时连接的客户端上限。
pub const MAX_CLIENTS: usize = 8;

/// client 在 `Clients` 中的条目。
struct Client {
    fd: i32,
    /// 是否已完成 HELLO → WELCOME 握手。
    greeted: bool,
    alive: bool,
}

/// 客户端连接集（固定数组，fd 由本结构唯一持有）。
pub struct Clients {
    list: [Client; MAX_CLIENTS],
}

impl Clients {
    pub fn new() -> Self {
        const EMPTY: Client = Client {
            fd: -1,
            greeted: false,
            alive: false,
        };
        Self {
            list: [EMPTY; MAX_CLIENTS],
        }
    }

    pub fn add(&mut self, fd: i32) -> Option<usize> {
        let slot = self.list.iter().position(|client| !client.alive)?;
        self.list[slot] = Client {
            fd,
            greeted: false,
            alive: true,
        };
        Some(slot)
    }

    /// 遍历连接槽位（事件循环构造 poll 数组用）；返回 `(槽位, fd)`。
    pub fn slots(&self) -> impl Iterator<Item = (usize, i32)> {
        self.list
            .iter()
            .enumerate()
            .filter_map(|(index, client)| client.alive.then_some((index, client.fd)))
    }

    /// 发送一条已编码帧；失败（对端异常 / 缓冲满）静默忽略——对端死亡会
    /// 经 EOF / POLLERR 路径回收。
    pub fn send(&self, index: usize, buffer: &[u8]) {
        let Some(client) = self.list.get(index).filter(|client| client.alive) else {
            return;
        };
        let _ = display_proto::send_message(client.fd, buffer);
    }

    fn remove(&mut self, index: usize) {
        if self.list[index].alive {
            // SAFETY: fd 由本结构持有且仅关闭一次。
            unsafe { ffi::close(self.list[index].fd) };
            self.list[index].alive = false;
        }
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
        let fd = shell.clients.list[index].fd;
        let mut passed = None;
        let received = display_proto::recv_message(fd, &mut buffer, &mut passed);
        if let Some(passed_fd) = passed {
            // 客户端不应给桌面发 fd；立即关闭防泄漏。
            // SAFETY: passed_fd 为内核刚安装的有效描述符。
            unsafe { ffi::close(passed_fd) };
        }
        match received {
            Ok(0) => return false,
            Ok(length) => {
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
            Err(error) if error == -ffi::EAGAIN => return true,
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
            if shell.clients.list[index].greeted || hello.version != display_proto::PROTOCOL_VERSION
            {
                return false;
            }
            let welcome = Welcome {
                version: display_proto::PROTOCOL_VERSION,
            };
            let mut frame = [0u8; 16];
            let Some(length) = welcome.encode(&mut frame) else {
                return false;
            };
            if display_proto::send_message_with_fd(
                shell.clients.list[index].fd,
                &frame[..length],
                shell.scanout.drm_fd(),
            )
            .is_err()
            {
                return false;
            }
            shell.clients.list[index].greeted = true;
            if !shell.markers.client {
                mark(b"desktop: client connected\n");
                shell.markers.client = true;
            }
            true
        }
        display_proto::CREATE_SURFACE if shell.clients.list[index].greeted => {
            let Some((create, title)) = CreateSurface::parse(payload) else {
                return false;
            };
            create_surface(index, &create, title, shell);
            true
        }
        display_proto::COMMIT if shell.clients.list[index].greeted => {
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
        display_proto::SET_TITLE if shell.clients.list[index].greeted => {
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
        display_proto::SET_BUFFER if shell.clients.list[index].greeted => {
            let Some(set_buffer) = SetBuffer::parse(payload) else {
                return false;
            };
            apply_set_buffer(index, &set_buffer, shell);
            true
        }
        display_proto::DESTROY_SURFACE if shell.clients.list[index].greeted => {
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
    let valid = create.gem_handle != 0
        && (1..=4096).contains(&width)
        && (1..=4096).contains(&height);
    // 内核 dumb pitch 恒为 width * 4。
    let size = width
        .checked_mul(4)
        .and_then(|pitch| pitch.checked_mul(height));
    let (Some(size), true) = (size, valid) else {
        reply(0, EINVAL);
        return;
    };
    let mapped = scanout::map_dumb_buffer(shell.scanout.drm_fd(), create.gem_handle, size);
    let Ok(pixels) = mapped else {
        reply(0, EINVAL);
        scanout::destroy_dumb(shell.scanout.drm_fd(), create.gem_handle);
        return;
    };
    let added = shell.windows.add(SurfaceDesc {
        client: index,
        gem_handle: create.gem_handle,
        pixels,
        map_size: size,
        width,
        height,
        decorated: create.flags & display_proto::SURFACE_FLAG_UNDECORATED == 0,
        title,
    });
    let Some((slot, surface_id)) = added else {
        // SAFETY: pixels/size 为刚完成的映射，handle 所有权在桌面。
        unsafe { ffi::munmap(pixels.cast(), size) };
        scanout::destroy_dumb(shell.scanout.drm_fd(), create.gem_handle);
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
    let drm_fd = shell.scanout.drm_fd();
    // 新 handle 所有权随消息转移给桌面：拒绝路径必须销毁它防泄漏。
    let reject = |handle: u32| {
        if handle != 0 {
            scanout::destroy_dumb(drm_fd, handle);
        }
    };
    let Some(slot) = shell.windows.by_surface(set_buffer.surface_id) else {
        reject(set_buffer.gem_handle);
        return;
    };
    let Some(window) = shell.windows.get(slot) else {
        reject(set_buffer.gem_handle);
        return;
    };
    if window.client != index {
        reject(set_buffer.gem_handle);
        return;
    }
    let width = set_buffer.width as usize;
    let height = set_buffer.height as usize;
    let valid = set_buffer.gem_handle != 0
        && (1..=4096).contains(&width)
        && (1..=4096).contains(&height);
    // 内核 dumb pitch 恒为 width * 4。
    let size = width
        .checked_mul(4)
        .and_then(|pitch| pitch.checked_mul(height));
    let (Some(size), true) = (size, valid) else {
        reject(set_buffer.gem_handle);
        return;
    };
    let Ok(pixels) = scanout::map_dumb_buffer(drm_fd, set_buffer.gem_handle, size) else {
        reject(set_buffer.gem_handle);
        return;
    };
    let old = window.outer_rect();
    let Some(window) = shell.windows.get_mut(slot) else {
        return;
    };
    window.apply_buffer(drm_fd, set_buffer.gem_handle, pixels, size, width, height);
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
    shell.windows.remove(slot, shell.scanout.drm_fd());
    shell.damage.add(shell.taskbar.strip_rect());
    if shell.windows.focused().is_none() {
        let top = shell.windows.top_visible();
        pointer::set_focus(shell.windows, shell.clients, shell.damage, top);
    }
}

/// 连接回收：关闭 fd、销毁该 client 的全部窗口（munmap + DESTROY_DUMB +
/// damage 旧区域）、修补焦点。
pub fn drop_client(index: usize, shell: &mut Shell) {
    let mut owned = [None; crate::window::MAX_WINDOWS];
    for (position, slot) in shell.windows.bottom_to_top().iter().enumerate() {
        if shell
            .windows
            .get(*slot)
            .is_some_and(|window| window.client == index)
        {
            owned[position] = Some(*slot);
        }
    }
    for slot in owned.into_iter().flatten() {
        if let Some(window) = shell.windows.get(slot) {
            shell.damage.add(window.outer_rect());
        }
        shell.windows.remove(slot, shell.scanout.drm_fd());
    }
    shell.damage.add(shell.taskbar.strip_rect());
    if shell.windows.focused().is_none() {
        let top = shell.windows.top_visible();
        pointer::set_focus(shell.windows, shell.clients, shell.damage, top);
    }
    shell.clients.remove(index);
}

fn mark(text: &[u8]) {
    // SAFETY: text 在 write 期间有效；fd 2 为 stderr（UART 日志）。
    unsafe { ffi::write(2, text.as_ptr().cast(), text.len()) };
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
