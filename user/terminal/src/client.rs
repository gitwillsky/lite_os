//! 桌面客户端：显示协议握手 + 单线程 poll 事件循环。
//!
//! 连接序列：`socket` → `connect`（桌面未 listen 时每秒重试，上限 60 次）→
//! `HELLO` → `WELCOME` + SCM_RIGHTS DRM fd → 在共享 fd 上建 dumb buffer →
//! `CREATE_SURFACE` → `SURFACE_CREATED` → spawn shell + 首帧 `COMMIT`。
//! 事件循环只 poll 两个 fd：PTY master 与桌面 socket；桌面经 `CONFIGURE`
//! 建议新尺寸时走 resize 事务换 backing buffer（见 [`crate::configure`]）。
//! 带启动命令启动时（`run(command)` 非空），建 surface 后 `SET_TITLE` 换成命令
//! 文本，首帧 `COMMIT` 后把命令当作键盘输入注入 PTY master 让 shell 执行。

use core::ptr;

use display_proto as proto;

use crate::{
    MAX_COMMAND_BYTES,
    atlas::{Atlas, FontMetrics},
    configure::handle_configure,
    ffi::{self, PollFd, SockaddrUn},
    input::{
        self, InputQueue, KeyboardState, MAX_KEY_BYTES, PTY_REPLY_EXPANSION, flush_input,
    },
    model::Model,
    pointer::{MAX_POINTER_BYTES, Pointer},
    render::{self, Surface},
    session::{read_pty, replay_boot_log, spawn_shell, terminate_child},
};

/// 窗口内容：初始 80×24 cell（32×64 像素）= 2560×1536；之后随 `CONFIGURE` 调整。
const COLUMNS: usize = 80;
const ROWS: usize = 24;
const WIDTH: u32 = 2560;
const HEIGHT: u32 = 1536;

const FRAME_INTERVAL_MS: u64 = 17;
const BLINK_INTERVAL_MS: u64 = 500;
const CONNECT_RETRY_MS: i32 = 1_000;
/// 桌面可能还没 listen；退避上限 60 次 ≈ 1 分钟，之后退出由桌面 respawn。
const CONNECT_RETRIES: usize = 60;

const TITLE: &[u8] = "终端".as_bytes();

/// 进程入口：握手、建 surface、spawn shell，然后进入事件循环。
///
/// `command` 为启动命令文本（argv[1..] join，见 `startup_command`），非空时建
/// surface 后 `SET_TITLE` 换成命令文本，并在首帧 `COMMIT` 后注入 PTY。
///
/// 返回进程退出码：正常结束（shell 退出 / `CLOSE_REQUEST`）为 0；
/// 握手失败、spawn 失败或桌面 socket EOF 为 1（桌面会 respawn）。
pub fn run(command: &[u8]) -> i32 {
    let Some(atlas) = Atlas::checked() else {
        report(b"terminal: atlas failed\n");
        return 1;
    };
    let metrics = atlas.metrics();
    let Some(socket) = connect() else {
        report(b"terminal: connect failed\n");
        return 1;
    };
    let Some(drm_fd) = handshake(socket) else {
        report(b"terminal: handshake failed\n");
        return 1;
    };
    let Some((mut surface, gem_handle)) = Surface::create(drm_fd, WIDTH, HEIGHT) else {
        report(b"terminal: surface failed\n");
        return 1;
    };
    // handle 所有权随本条消息转移给桌面；此后本进程绝不 DESTROY_DUMB。
    let Some(surface_id) = create_surface(socket, gem_handle) else {
        report(b"terminal: create failed\n");
        return 1;
    };
    if !command.is_empty() {
        send_set_title(socket, surface_id, command);
    }
    report(b"terminal: connected\n");

    let Some(mut model) = Model::new(COLUMNS, ROWS) else {
        return 1;
    };
    model.feed(b"\x1b[2J\x1b[HLiteOS\n\n", |_| {});
    replay_boot_log(&mut model);

    let Some((master, child)) = spawn_shell(
        COLUMNS,
        ROWS,
        u16::try_from(WIDTH).unwrap_or(0),
        u16::try_from(HEIGHT).unwrap_or(0),
    ) else {
        report(b"terminal: spawn failed\n");
        return 1;
    };
    model.begin_shell_session();
    render::render_full(&mut surface, &model, &atlas, metrics, true);
    model.clear_all_dirty();
    if send_commit(socket, surface_id, &[]).is_err() {
        terminate_child(child);
        return 1;
    }
    inject_command(master, command);
    report(b"terminal: shell spawned\n");

    // 握手期间 socket 保持阻塞以便顺序收发；进入事件循环前切非阻塞。
    if unsafe { ffi::fcntl(socket, ffi::F_SETFL, ffi::O_NONBLOCK) } < 0 {
        terminate_child(child);
        return 1;
    }
    event_loop(
        &Session {
            socket,
            master,
            child,
            drm_fd,
            surface_id,
        },
        &mut surface,
        &atlas,
        &mut model,
        metrics,
    )
}

fn connect() -> Option<i32> {
    let path = proto::SOCKET_PATH.as_bytes();
    let mut address = SockaddrUn {
        family: ffi::AF_UNIX as u16,
        path: [0; 108],
    };
    address.path[..path.len()].copy_from_slice(path);
    for _ in 0..CONNECT_RETRIES {
        let fd = unsafe { ffi::socket(ffi::AF_UNIX, ffi::SOCK_STREAM | ffi::O_CLOEXEC, 0) };
        if fd < 0 {
            return None;
        }
        if unsafe { ffi::connect(fd, &address, size_of::<SockaddrUn>() as u32) } == 0 {
            return Some(fd);
        }
        unsafe { ffi::close(fd) };
        unsafe { ffi::poll(ptr::null_mut(), 0, CONNECT_RETRY_MS) };
    }
    None
}

/// `HELLO` → `WELCOME`：收回协议版本确认与 SCM_RIGHTS 传来的共享 DRM fd。
///
/// WELCOME 携带 fd，内核 SCM_RIGHTS barrier 会把它拆成「1 字节 + fd」与
/// 「剩余字节」两段，必须用 [`proto::recv_frame_blocking`] 拼帧。
fn handshake(socket: i32) -> Option<i32> {
    let mut buf = [0u8; proto::MAX_MESSAGE];
    let length = proto::Hello {
        version: proto::PROTOCOL_VERSION,
    }
    .encode(&mut buf)?;
    if proto::send_message(socket, &buf[..length]).is_err() {
        report(b"terminal: hs send failed\n");
        return None;
    }
    let mut drm_fd = None;
    let count = match proto::recv_frame_blocking(socket, &mut buf, &mut drm_fd) {
        Ok(count) => count,
        Err(_) => {
            report(b"terminal: hs recv failed\n");
            return None;
        }
    };
    if count == 0 {
        report(b"terminal: hs eof\n");
        return None;
    }
    let Some((header, payload)) = proto::parse_header(&buf[..count]) else {
        report(b"terminal: hs frame failed\n");
        return None;
    };
    if header.kind != proto::WELCOME {
        report(b"terminal: hs kind failed\n");
        return None;
    }
    let welcome = proto::Welcome::parse(payload)?;
    if welcome.version != proto::PROTOCOL_VERSION {
        report(b"terminal: hs version failed\n");
        return None;
    }
    if drm_fd.is_none() {
        report(b"terminal: hs nofd\n");
    }
    drm_fd
}

/// `CREATE_SURFACE` → `SURFACE_CREATED`，返回分配到的 surface id（`error == 0`）。
fn create_surface(socket: i32, gem_handle: u32) -> Option<u32> {
    let mut buf = [0u8; proto::MAX_MESSAGE];
    let length = proto::CreateSurface::encode(&mut buf, WIDTH, HEIGHT, gem_handle, 0, TITLE)?;
    proto::send_message(socket, &buf[..length]).ok()?;
    let mut unused_fd = None;
    let count = proto::recv_frame_blocking(socket, &mut buf, &mut unused_fd).ok()?;
    if let Some(fd) = unused_fd {
        // 协议上只有 WELCOME 携带 fd；此处收到属对端异常，立即关闭防泄漏。
        unsafe { ffi::close(fd) };
    }
    if count == 0 {
        return None;
    }
    let (header, payload) = proto::parse_header(&buf[..count])?;
    if header.kind != proto::SURFACE_CREATED {
        return None;
    }
    let created = proto::SurfaceCreated::parse(payload)?;
    (created.error == 0).then_some(created.surface_id)
}

/// 有启动命令时把窗口标题换成命令文本；标题属装饰性消息，发送失败静默忽略。
fn send_set_title(socket: i32, surface_id: u32, title: &[u8]) {
    let mut buf = [0u8; 16 + MAX_COMMAND_BYTES];
    if let Some(length) = proto::SetTitle::encode(&mut buf, surface_id, title) {
        let _ = proto::send_message(socket, &buf[..length]);
    }
}

/// 把启动命令当作键盘输入写进 PTY master：canonical 模式下 shell 收到末尾
/// `\r` 才执行整行，命令结束后 shell 仍在，输出留在窗口里。master 是
/// O_NONBLOCK，写不进或部分写都静默忽略（用户仍可手动输入）。
fn inject_command(master: i32, command: &[u8]) {
    if command.is_empty() {
        return;
    }
    let mut line = [0u8; MAX_COMMAND_BYTES + 1];
    line[..command.len()].copy_from_slice(command);
    line[command.len()] = b'\r';
    let line = &line[..command.len() + 1];
    let mut written = 0;
    while written < line.len() {
        let count = unsafe { ffi::write(master, line[written..].as_ptr().cast(), line.len() - written) };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            return;
        }
    }
}

/// 事件循环期间持有的 fd 与 surface 标识；client 是它们的唯一 owner。
pub(crate) struct Session {
    pub(crate) socket: i32,
    pub(crate) master: i32,
    pub(crate) child: i32,
    /// 共享 DRM fd；初始建 surface 之外，resize 事务复用它创建新 dumb buffer。
    pub(crate) drm_fd: i32,
    pub(crate) surface_id: u32,
}

fn event_loop(
    session: &Session,
    surface: &mut Surface,
    atlas: &Atlas,
    model: &mut Model,
    metrics: FontMetrics,
) -> i32 {
    let Session {
        socket,
        master,
        child,
        surface_id,
        ..
    } = *session;
    let mut keyboard = KeyboardState::default();
    let mut pointer = Pointer::new();
    let mut input = InputQueue::new();
    let mut focused = true;
    let mut render_due = None::<u64>;
    let mut blink_due = None::<u64>;
    let mut last_present = ffi::monotonic_milliseconds();
    // socket 接收缓冲：stream socket 下多条帧可合并到达、单条帧也可被拆分，
    // 逐帧切出后保留残余字节等下一次 poll。
    let mut rx = [0u8; proto::MAX_MESSAGE];
    let mut rx_len = 0usize;
    loop {
        let now = ffi::monotonic_milliseconds();
        if render_due.is_some_and(|deadline| deadline <= now) {
            if let Some(damage) = render::present(surface, model, atlas, metrics, focused)
                && send_commit(socket, surface_id, &damage.rects[..damage.count]).is_err()
            {
                terminate_child(child);
                return 1;
            }
            render_due = None;
            last_present = now;
        }
        if blink_due.is_some_and(|deadline| deadline <= now) {
            if focused && model.toggle_blink() {
                schedule_render(&mut render_due, last_present, now);
                blink_due = Some(now.saturating_add(BLINK_INTERVAL_MS));
            } else {
                blink_due = None;
            }
        }

        let timeout = timeout(render_due, blink_due, ffi::monotonic_milliseconds());
        let mut descriptors = [
            PollFd {
                fd: master,
                events: if input.remaining() >= PTY_REPLY_EXPANSION {
                    ffi::POLLIN
                } else {
                    0
                } | if input.is_empty() { 0 } else { ffi::POLLOUT },
                returned: 0,
            },
            PollFd {
                fd: socket,
                events: ffi::POLLIN,
                returned: 0,
            },
        ];
        let ready = loop {
            let result = unsafe { ffi::poll(descriptors.as_mut_ptr(), 2, timeout) };
            if result < 0 && ffi::errno() == ffi::EINTR {
                continue;
            }
            break result;
        };
        if ready < 0 {
            terminate_child(child);
            return 1;
        }
        let now = ffi::monotonic_milliseconds();
        let mut closed = false;
        if descriptors[0].returned & (ffi::POLLIN | ffi::POLLERR | ffi::POLLHUP) != 0 {
            let (changed, ended) = read_pty(master, model, &mut input);
            // Device-status/attributes 查询属于 PTY request/reply；同轮立即写回可避免全屏程序
            // 等待下一次 POLLOUT edge 或超时，键盘/鼠标输入仍与回复共用唯一有界队列。
            flush_input(master, &mut input);
            closed = ended;
            if changed {
                schedule_render(&mut render_due, last_present, now);
                if blink_due.is_none() && focused && model.has_blinking_cells() {
                    blink_due = Some(now.saturating_add(BLINK_INTERVAL_MS));
                }
            }
        }
        if descriptors[0].returned & ffi::POLLOUT != 0 {
            flush_input(master, &mut input);
        }
        if descriptors[1].returned & (ffi::POLLIN | ffi::POLLERR | ffi::POLLHUP) != 0 {
            match drain_socket(socket, &mut rx, &mut rx_len) {
                SocketStatus::Open => {}
                // 桌面已退出（EOF）或协议/读取错误：本进程随之退出，由桌面 respawn。
                SocketStatus::Closed | SocketStatus::Fatal => {
                    terminate_child(child);
                    return 1;
                }
            }
            let mut offset = 0;
            let mut close_requested = false;
            loop {
                let frame = match buffered_frame(&rx[offset..rx_len]) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(()) => {
                        terminate_child(child);
                        return 1;
                    }
                };
                let Some((header, payload)) = proto::parse_header(&rx[offset..offset + frame])
                else {
                    terminate_child(child);
                    return 1;
                };
                if header.kind == proto::CONFIGURE {
                    handle_configure(payload, session, surface, atlas, model, metrics, focused);
                } else {
                    dispatch(
                        header.kind,
                        payload,
                        &mut input,
                        &mut keyboard,
                        &mut pointer,
                        model,
                        metrics,
                        master,
                        &mut focused,
                        &mut render_due,
                        &mut blink_due,
                        last_present,
                        now,
                        &mut close_requested,
                    );
                }
                offset += frame;
            }
            rx.copy_within(offset..rx_len, 0);
            rx_len -= offset;
            if close_requested {
                terminate_child(child);
                return 0;
            }
        }
        if render_due.is_some_and(|deadline| deadline <= now) {
            if let Some(damage) = render::present(surface, model, atlas, metrics, focused)
                && send_commit(socket, surface_id, &damage.rects[..damage.count]).is_err()
            {
                terminate_child(child);
                return 1;
            }
            render_due = None;
            last_present = now;
        }
        if closed {
            terminate_child(child);
            return 0;
        }
    }
}

/// 分发一条桌面 → 客户端消息。输入队列容量不足的输入消息直接丢弃（对齐原
/// reactor 的 poll 门控：ring 满时设备事件整批跳过）。
#[expect(clippy::too_many_arguments)]
fn dispatch(
    kind: u32,
    payload: &[u8],
    input: &mut InputQueue,
    keyboard: &mut KeyboardState,
    pointer: &mut Pointer,
    model: &mut Model,
    metrics: FontMetrics,
    master: i32,
    focused: &mut bool,
    render_due: &mut Option<u64>,
    blink_due: &mut Option<u64>,
    last_present: u64,
    now: u64,
    close_requested: &mut bool,
) {
    match kind {
        proto::INPUT_KEY => {
            let Some(key) = proto::InputKey::parse(payload) else {
                return;
            };
            if input.remaining() >= MAX_KEY_BYTES {
                input::handle_key(input, keyboard, key.code, key.value, model);
                flush_input(master, input);
            }
        }
        proto::INPUT_POINTER => {
            let Some(event) = proto::InputPointer::parse(payload) else {
                return;
            };
            if input.remaining() >= MAX_POINTER_BYTES {
                pointer.handle(input, model, metrics, &event);
                flush_input(master, input);
            }
        }
        proto::INPUT_SYNC_RESET => {
            input::reset_modifiers(keyboard);
            pointer.reset();
        }
        proto::FOCUS => {
            let Some(focus) = proto::Focus::parse(payload) else {
                return;
            };
            let next = focus.focused != 0;
            if next != *focused {
                *focused = next;
                // 光标绘制随焦点开关；全量标脏让下一次 present 重画光标所在 cell。
                model.mark_all();
                schedule_render(render_due, last_present, now);
                *blink_due = if next && model.has_blinking_cells() {
                    Some(now.saturating_add(BLINK_INTERVAL_MS))
                } else {
                    None
                };
            }
        }
        proto::CLOSE_REQUEST => *close_requested = true,
        _ => {}
    }
}

enum SocketStatus {
    Open,
    Closed,
    Fatal,
}

/// 把 socket 上当前可得的字节全部搬入接收缓冲。
fn drain_socket(socket: i32, rx: &mut [u8; proto::MAX_MESSAGE], length: &mut usize) -> SocketStatus {
    loop {
        if *length == rx.len() {
            // 缓冲已满：存在完整帧则先交给分发消化，否则是对端违约的协议错误。
            return match buffered_frame(rx) {
                Ok(Some(_)) => SocketStatus::Open,
                Ok(None) | Err(()) => SocketStatus::Fatal,
            };
        }
        let count = unsafe { ffi::read(socket, rx[*length..].as_mut_ptr().cast(), rx.len() - *length) };
        if count > 0 {
            *length += count as usize;
        } else if count == 0 {
            return SocketStatus::Closed;
        } else if ffi::errno() == ffi::EINTR {
            continue;
        } else if ffi::errno() == ffi::EAGAIN {
            return SocketStatus::Open;
        } else {
            return SocketStatus::Fatal;
        }
    }
}

/// 已缓冲字节中第一条完整帧的总长；帧头未收齐或帧体不完整时返回 `Ok(None)`。
/// 帧头声明长度越界视为协议错误（`Err(())`），调用方按 `SocketStatus::Fatal`
/// 路径退出。
fn buffered_frame(buffered: &[u8]) -> Result<Option<usize>, ()> {
    if buffered.len() < proto::HEADER_LEN {
        return Ok(None);
    }
    let declared = u32::from_le_bytes(buffered[0..4].try_into().map_err(|_| ())?) as usize;
    if !(proto::HEADER_LEN..=proto::MAX_MESSAGE).contains(&declared) {
        return Err(());
    }
    Ok((buffered.len() >= declared).then_some(declared))
}

/// 发送一条 `COMMIT`；`rects` 为空表示整幅 damage。
pub(crate) fn send_commit(socket: i32, surface_id: u32, rects: &[[u16; 4]]) -> Result<(), ()> {
    let mut buf = [0u8; 16 + 8 * proto::MAX_DAMAGE_RECTS];
    let length = proto::Commit::encode(&mut buf, surface_id, rects).ok_or(())?;
    proto::send_message(socket, &buf[..length]).map_err(|_| ())
}

fn schedule_render(deadline: &mut Option<u64>, last_present: u64, now: u64) {
    if deadline.is_none() {
        *deadline = Some(if now.saturating_sub(last_present) >= FRAME_INTERVAL_MS {
            now
        } else {
            last_present.saturating_add(FRAME_INTERVAL_MS)
        });
    }
}

fn timeout(render: Option<u64>, blink: Option<u64>, now: u64) -> i32 {
    let deadline = [render, blink].into_iter().flatten().min();
    deadline.map_or(-1, |deadline| {
        i32::try_from(deadline.saturating_sub(now)).unwrap_or(i32::MAX)
    })
}

/// 向 stderr 完整写入一行启动 marker / 错误（供 gate 匹配），`EINTR` 重试。
fn report(message: &[u8]) {
    let mut written = 0;
    while written < message.len() {
        let count = unsafe {
            ffi::write(
                2,
                message[written..].as_ptr().cast(),
                message.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            break;
        }
    }
}
