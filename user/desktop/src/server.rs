//! 显示服务器：`/run/display.sock` 监听、单线程 poll 事件循环（listen
//! socket + client sockets + keyboard/tablet evdev），每轮处理完所有就绪
//! 事件后统一合成一次并 `DIRTYFB` 提交。协议消息处理见 `clients`，指针
//! 语义见 `pointer`，任务栏见 `taskbar`。
//!
//! poll 超时取 min(terminal respawn 重试, 任务栏时钟到下一整分钟的毫秒数)，
//! 保证时钟分钟翻转能及时重画。

use crate::{
    clients::{self, Clients, Markers, MAX_CLIENTS},
    compositor::{self, Damage},
    cursor::Cursor,
    ffi,
    ffi::{PollFd, SockaddrUn},
    input::Input,
    pointer::PointerShell,
    scanout::{Rect, Scanout},
    shutdown,
    startmenu::StartMenu,
    supervisor::Supervisor,
    taskbar::Taskbar,
    uifont::UiFont,
    wallpaper::Wallpaper,
    window::Windows,
};

/// 桌面主循环的入口：modeset 成功 → 从 `/usr/share/liteos/` 加载字体 / 壁纸 /
/// 光标资产 → 监听 socket → 拉起 terminal → 事件循环。
///
/// 只在启动阶段失败时返回 `Err(())`（无 GPU / socket 不可用 / 资产缺失或校验
/// 失败），由 `main` 退避重试；进入事件循环后不返回。
pub fn run() -> Result<(), ()> {
    let mut scanout = Scanout::open()?;
    let mode = scanout.mode();
    clients::mark_mode(mode.width, mode.height);
    let font = UiFont::open().ok_or(())?;
    let wallpaper = Wallpaper::open(mode).ok_or(())?;
    let cursor = Cursor::open().ok_or(())?;
    let listen = listen_socket()?;
    let mut clients = Clients::new();
    let mut windows = Windows::new();
    let mut damage = Damage::new();
    let mut input = Input::open(mode.width as i32, mode.height as i32);
    let mut supervisor = Supervisor::new();
    let mut taskbar = Taskbar::new(mode.width as i32, mode.height as i32);
    let mut startmenu = StartMenu::load(mode.height as i32);
    let mut markers = Markers::new();
    // 关机流程：开始菜单 `关机` 项置位后画一次关机画面并 fork /bin/shutdown，
    // 此后停止响应输入（画面保持，等 init 关机）。
    let mut shutdown_requested = false;
    let mut shutting_down = false;
    // 首帧提交后置真：sysinit 的启动画面只接管一次。
    let mut splash_dismissed = false;
    // 首帧全屏重画（含壁纸、任务栏与光标）。
    damage.add(Rect::new(0, 0, mode.width as i32, mode.height as i32));
    supervisor.ensure_minimum();
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
        for (index, fd) in clients.slots() {
            descriptors[3 + index].fd = fd;
            descriptors[3 + index].events = ffi::POLLIN;
        }
        // terminal 待 respawn 时给 poll 加 500ms 超时驱动重试；同时用“到下一
        // 整分钟”的毫秒数约束超时，保证任务栏时钟分钟翻转能及时重画。
        let mut timeout = taskbar.ms_until_next_minute();
        if supervisor.waiting() {
            timeout = timeout.min(500);
        }
        // SAFETY: descriptors 在 poll 期间有效。
        let ready = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), timeout) };
        if ready < 0 && ffi::errno() != ffi::EINTR {
            continue;
        }
        let focus_before = windows.focused();
        if descriptors[0].returned & ffi::POLLIN != 0 {
            accept_clients(listen, &mut clients);
        }
        for index in 0..MAX_CLIENTS {
            if descriptors[3 + index].returned == 0 {
                continue;
            }
            let keep = {
                let mut shell = clients::Shell {
                    clients: &mut clients,
                    windows: &mut windows,
                    damage: &mut damage,
                    scanout: &scanout,
                    taskbar: &mut taskbar,
                    markers: &mut markers,
                };
                clients::service_client(index, &mut shell)
            };
            if !keep {
                let mut shell = clients::Shell {
                    clients: &mut clients,
                    windows: &mut windows,
                    damage: &mut damage,
                    scanout: &scanout,
                    taskbar: &mut taskbar,
                    markers: &mut markers,
                };
                clients::drop_client(index, &mut shell);
            }
        }
        if descriptors[1].returned != 0 && !shutting_down {
            input.poll_keyboard(&windows, &clients);
        }
        if descriptors[2].returned != 0 && !shutting_down {
            let mut shell = PointerShell {
                windows: &mut windows,
                clients: &clients,
                damage: &mut damage,
                taskbar: &mut taskbar,
                supervisor: &mut supervisor,
                startmenu: &mut startmenu,
                shutdown: &mut shutdown_requested,
                screen_width: mode.width as i32,
                screen_height: mode.height as i32,
            };
            input.poll_tablet(&mut shell);
        }
        if shutdown_requested && !shutting_down {
            shutting_down = true;
            damage.clear();
            shutdown::enter(&mut scanout, &font);
        }
        supervisor.reap();
        supervisor.ensure_minimum();
        // client 断连等窗口消失路径后，取消悬空的拖动 / 按钮按下态。
        input.validate_drag(&windows, &mut damage);
        // 焦点变化会改变任务栏窗口按钮的按下态：重画整条任务栏。
        if windows.focused() != focus_before {
            damage.add(taskbar.strip_rect());
        }
        // 时钟分钟翻转时只 damage 时钟矩形。
        taskbar.tick(&mut damage);
        if !damage.is_empty() && !shutting_down {
            compositor::composite(
                &mut scanout,
                &compositor::Layers {
                    windows: &windows,
                    font: &font,
                    wallpaper: &wallpaper,
                    taskbar: &taskbar,
                    startmenu: &startmenu,
                    cursor: &cursor,
                },
                &compositor::Overlays {
                    outline: input.resize_outline(),
                    armed: input.armed_button(),
                    cursor: (input.cursor_x, input.cursor_y),
                },
                &damage,
            );
            damage.clear();
            if !splash_dismissed {
                splash_dismissed = true;
                dismiss_splash();
            }
        }
    }
}

/// 首帧提交后结束 sysinit 的启动画面：读 `/run/splash.pid` 发 SIGTERM 并摘除
/// pid 文件。文件不存在（无 GPU 恢复路径、splash 未启动）时静默跳过——splash
/// 是纯装饰，桌面不依赖它的任何状态。
fn dismiss_splash() {
    let mut text = [0u8; 16];
    // SAFETY: 静态 NUL 结尾路径；text 在 read 期间可写。
    let fd = unsafe { ffi::open(ffi::c_str(b"/run/splash.pid\0"), ffi::O_RDONLY) };
    if fd < 0 {
        return;
    }
    // SAFETY: fd 为本函数打开的有效描述符。
    let length = unsafe { ffi::read(fd, text.as_mut_ptr().cast(), text.len()) };
    unsafe { ffi::close(fd) };
    let mut pid = 0i32;
    for byte in text.iter().take(length.max(0) as usize) {
        if !byte.is_ascii_digit() {
            break;
        }
        pid = pid.saturating_mul(10).saturating_add(i32::from(byte - b'0'));
    }
    if pid > 1 {
        // SAFETY: kill 只向 splash 记录的 pid 投递 SIGTERM。
        unsafe { ffi::kill(pid, ffi::SIGTERM) };
    }
    // SAFETY: 静态 NUL 结尾路径；pid 文件已由桌面消费。
    unsafe { ffi::unlink(ffi::c_str(b"/run/splash.pid\0")) };
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
