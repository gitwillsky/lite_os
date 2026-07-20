//! 显示服务器：`/run/display.sock` 监听、客户端握手与 surface 生命周期、
//! 单线程 poll 事件循环（listen socket + client sockets + keyboard/tablet
//! evdev），每轮处理完所有就绪事件后统一合成一次并 `DIRTYFB` 提交。
//!
//! 协议帧假设一次 `recv_message` 返回整数条完整帧（发送方单条 write、
//! 帧 ≤ [`MAX_MESSAGE`] 字节，unix stream 本地传输下成立）；读到截断帧视为
//! 协议错误并断开该客户端。客户端不会向桌面发 fd，握手外收到的 fd 立即
//! close 防泄漏。

use display_proto::{
    self, Commit, CreateSurface, DestroySurface, Hello, SetTitle, SurfaceCreated, Welcome,
};

use crate::{
    atlas::Atlas,
    chrome,
    compositor::{self, Damage},
    ffi,
    ffi::{PollFd, SockaddrUn},
    input::{self, Input},
    scanout::{Rect, Scanout},
    supervisor::Supervisor,
    window::{SurfaceDesc, Windows},
};

/// 同时连接的客户端上限（第一期只有 terminal）。
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
    fn new() -> Self {
        const EMPTY: Client = Client {
            fd: -1,
            greeted: false,
            alive: false,
        };
        Self {
            list: [EMPTY; MAX_CLIENTS],
        }
    }

    fn add(&mut self, fd: i32) -> Option<usize> {
        let slot = self.list.iter().position(|client| !client.alive)?;
        self.list[slot] = Client {
            fd,
            greeted: false,
            alive: true,
        };
        Some(slot)
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
struct Markers {
    client: bool,
    surface: bool,
}

/// 事件处理共享的可变状态束（避免函数签名参数过长）。
struct Shell<'a> {
    clients: &'a mut Clients,
    windows: &'a mut Windows,
    damage: &'a mut Damage,
    scanout: &'a Scanout,
    markers: &'a mut Markers,
}

/// 桌面主循环的入口：modeset 成功 → 监听 socket → 拉起 terminal → 事件循环。
///
/// 只在启动阶段失败时返回 `Err(())`（无 GPU / socket 不可用），由 `main`
/// 退避重试；进入事件循环后不返回。
pub fn run() -> Result<(), ()> {
    let mut scanout = Scanout::open()?;
    let mode = scanout.mode();
    mark_mode(mode.width, mode.height);
    let atlas = Atlas::checked().ok_or(())?;
    let listen = listen_socket()?;
    let mut clients = Clients::new();
    let mut windows = Windows::new();
    let mut damage = Damage::new();
    let mut input = Input::open(mode.width as i32, mode.height as i32);
    let mut supervisor = Supervisor::new();
    let mut markers = Markers {
        client: false,
        surface: false,
    };
    // 首帧全屏重画（含背景与光标）。
    damage.add(Rect::new(0, 0, mode.width as i32, mode.height as i32));
    supervisor.ensure_terminal();
    loop {
        let mut descriptors = [PollFd {
            fd: -1,
            events: 0,
            returned: 0,
        }; 3 + MAX_CLIENTS];
        descriptors[0].fd = listen;
        descriptors[0].events = ffi::POLLIN;
        descriptors[1].fd = input.keyboard_fd;
        descriptors[1].events = ffi::POLLIN;
        descriptors[2].fd = input.tablet_fd;
        descriptors[2].events = ffi::POLLIN;
        for (index, client) in clients.list.iter().enumerate() {
            if client.alive {
                descriptors[3 + index].fd = client.fd;
                descriptors[3 + index].events = ffi::POLLIN;
            }
        }
        // terminal 待 respawn 时给 poll 加超时以驱动重试，否则无限等待事件。
        let timeout = if supervisor.waiting() { 500 } else { -1 };
        // SAFETY: descriptors 在 poll 期间有效。
        let ready = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), timeout) };
        if ready < 0 && ffi::errno() != ffi::EINTR {
            continue;
        }
        if descriptors[0].returned & ffi::POLLIN != 0 {
            accept_clients(listen, &mut clients);
        }
        for index in 0..MAX_CLIENTS {
            if descriptors[3 + index].returned == 0 {
                continue;
            }
            let keep = {
                let mut shell = Shell {
                    clients: &mut clients,
                    windows: &mut windows,
                    damage: &mut damage,
                    scanout: &scanout,
                    markers: &mut markers,
                };
                service_client(index, &mut shell)
            };
            if !keep {
                drop_client(index, &mut clients, &mut windows, &mut damage, &scanout);
            }
        }
        if descriptors[1].returned != 0 {
            input.poll_keyboard(&windows, &clients);
        }
        if descriptors[2].returned != 0 {
            input.poll_tablet(
                &mut windows,
                &clients,
                &mut damage,
                mode.width as i32,
                mode.height as i32,
            );
        }
        supervisor.reap();
        supervisor.ensure_terminal();
        if !damage.is_empty() {
            compositor::composite(
                &mut scanout,
                &windows,
                &atlas,
                input.cursor_x,
                input.cursor_y,
                &damage,
            );
            damage.clear();
        }
    }
}

/// `SOCK_NONBLOCK` listen socket：bind 前 unlink 陈旧节点。
fn listen_socket() -> Result<i32, ()> {
    // SAFETY: 静态 NUL 结尾路径。
    unsafe { ffi::unlink(ffi::c_str(b"/run/display.sock\0")) };
    let fd = unsafe {
        ffi::socket(
            ffi::AF_UNIX,
            ffi::SOCK_STREAM | ffi::SOCK_NONBLOCK | ffi::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(());
    }
    let path = display_proto::SOCKET_PATH.as_bytes();
    let mut address = SockaddrUn {
        family: ffi::AF_UNIX as u16,
        path: [0; 108],
    };
    address.path[..path.len()].copy_from_slice(path);
    let length = (2 + path.len() + 1) as u32;
    // SAFETY: address 为有效的 sockaddr_un，length 覆盖到路径 NUL。
    let bound = unsafe { ffi::bind(fd, &address, length) };
    // SAFETY: fd 为本函数打开的 socket。
    if bound < 0 || unsafe { ffi::listen(fd, MAX_CLIENTS as i32) } < 0 {
        unsafe { ffi::close(fd) };
        return Err(());
    }
    Ok(fd)
}

fn accept_clients(listen: i32, clients: &mut Clients) {
    loop {
        // SAFETY: 不需要对端地址，flags 仅 NONBLOCK|CLOEXEC。
        let fd = unsafe {
            ffi::accept4(
                listen,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                ffi::SOCK_NONBLOCK | ffi::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            if ffi::errno() == ffi::EINTR {
                continue;
            }
            return;
        }
        if clients.add(fd).is_none() {
            // 客户端满：直接拒绝（对端读 EOF 后自行退出）。
            // SAFETY: fd 未被登记，由本函数关闭。
            unsafe { ffi::close(fd) };
        }
    }
}

/// 消费一个 client socket 上的所有待读消息；返回 `false` 表示连接应回收
/// （EOF / 协议错误 / 系统错误）。
fn service_client(index: usize, shell: &mut Shell) -> bool {
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
    let mapped = crate::scanout::map_dumb_buffer(shell.scanout.drm_fd(), create.gem_handle, size);
    let Ok(pixels) = mapped else {
        reply(0, EINVAL);
        crate::scanout::destroy_dumb(shell.scanout.drm_fd(), create.gem_handle);
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
        crate::scanout::destroy_dumb(shell.scanout.drm_fd(), create.gem_handle);
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
    input::set_focus(shell.windows, shell.clients, shell.damage, Some(slot));
}

/// 销毁窗口并修补焦点（焦点丢失时回落到栈顶窗口）。
fn destroy_window(slot: usize, shell: &mut Shell) {
    if let Some(window) = shell.windows.get(slot) {
        shell.damage.add(window.outer_rect());
    }
    shell.windows.remove(slot, shell.scanout.drm_fd());
    if shell.windows.focused().is_none() {
        let top = shell.windows.top();
        input::set_focus(shell.windows, shell.clients, shell.damage, top);
    }
}

/// 连接回收：关闭 fd、销毁该 client 的全部窗口（munmap + DESTROY_DUMB +
/// damage 旧区域）、修补焦点。
fn drop_client(
    index: usize,
    clients: &mut Clients,
    windows: &mut Windows,
    damage: &mut Damage,
    scanout: &Scanout,
) {
    let mut owned = [None; crate::window::MAX_WINDOWS];
    for (position, slot) in windows.bottom_to_top().iter().enumerate() {
        if windows
            .get(*slot)
            .is_some_and(|window| window.client == index)
        {
            owned[position] = Some(*slot);
        }
    }
    for slot in owned.into_iter().flatten() {
        if let Some(window) = windows.get(slot) {
            damage.add(window.outer_rect());
        }
        windows.remove(slot, scanout.drm_fd());
    }
    if windows.focused().is_none() {
        let top = windows.top();
        input::set_focus(windows, clients, damage, top);
    }
    clients.remove(index);
}

fn mark(text: &[u8]) {
    // SAFETY: text 在 write 期间有效；fd 2 为 stderr（UART 日志）。
    unsafe { ffi::write(2, text.as_ptr().cast(), text.len()) };
}

fn mark_mode(width: usize, height: usize) {
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
