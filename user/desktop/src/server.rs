//! 显示服务器：Unix socket、evdev 与客户端连接的单线程 poll 事件循环。

use linux_uapi::{
    process::{self, Pid, Signal},
    unix::{self, PollEvents, PollFd},
};
use std::{
    fs,
    os::fd::AsFd,
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

use crate::{
    clients::{self, Clients, Markers},
    compositor::{self, Damage},
    cursor::Cursor,
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

/// 启动桌面资源并进入事件循环。
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
    let mut shutdown_requested = false;
    let mut shutting_down = false;
    let mut splash_dismissed = false;
    // 两个 Vec 跨轮次复用容量，客户端事件热路径不会重复分配。
    let mut descriptors = Vec::new();
    let mut client_descriptors = Vec::new();
    damage.add(Rect::new(0, 0, mode.width as i32, mode.height as i32));
    supervisor.ensure_minimum();

    loop {
        descriptors.clear();
        client_descriptors.clear();
        let active_clients = clients.active_len();
        descriptors
            .try_reserve(3 + active_clients)
            .map_err(|_| ())?;
        client_descriptors
            .try_reserve(active_clients)
            .map_err(|_| ())?;

        descriptors.push(PollFd::new(listen.as_fd(), PollEvents::READ));
        let keyboard_descriptor = input.keyboard_fd().map(|fd| {
            let index = descriptors.len();
            descriptors.push(PollFd::new(fd, PollEvents::READ));
            index
        });
        let tablet_descriptor = input.tablet_fd().map(|fd| {
            let index = descriptors.len();
            descriptors.push(PollFd::new(fd, PollEvents::READ));
            index
        });
        for (client, stream) in clients.slots() {
            let descriptor = descriptors.len();
            descriptors.push(PollFd::new(stream.as_fd(), PollEvents::READ));
            client_descriptors.push((client, descriptor));
        }

        let mut timeout = taskbar.ms_until_next_minute();
        if supervisor.waiting() {
            timeout = timeout.min(500);
        }
        match unix::poll(
            &mut descriptors,
            Some(Duration::from_millis(timeout.max(0) as u64)),
        ) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => continue,
        }

        let focus_before = windows.focused();
        if descriptors[0].returned().contains(PollEvents::READ) {
            accept_clients(&listen, &mut clients);
        }
        for &(client, descriptor) in &client_descriptors {
            if descriptors[descriptor].returned() == PollEvents::EMPTY {
                continue;
            }
            let keep = clients::service_client(
                client,
                &mut clients::Shell {
                    clients: &mut clients,
                    windows: &mut windows,
                    damage: &mut damage,
                    scanout: &scanout,
                    taskbar: &mut taskbar,
                    markers: &mut markers,
                },
            );
            if !keep {
                clients::drop_client(
                    client,
                    &mut clients::Shell {
                        clients: &mut clients,
                        windows: &mut windows,
                        damage: &mut damage,
                        scanout: &scanout,
                        taskbar: &mut taskbar,
                        markers: &mut markers,
                    },
                );
            }
        }
        if keyboard_descriptor
            .is_some_and(|index| descriptors[index].returned().contains(PollEvents::READ))
            && !shutting_down
        {
            input.poll_keyboard(&windows, &clients);
        }
        if tablet_descriptor
            .is_some_and(|index| descriptors[index].returned().contains(PollEvents::READ))
            && !shutting_down
        {
            input.poll_tablet(&mut PointerShell {
                windows: &mut windows,
                clients: &clients,
                damage: &mut damage,
                taskbar: &mut taskbar,
                supervisor: &mut supervisor,
                startmenu: &mut startmenu,
                shutdown: &mut shutdown_requested,
                screen_width: mode.width as i32,
                screen_height: mode.height as i32,
            });
        }
        if shutdown_requested && !shutting_down {
            shutting_down = true;
            damage.clear();
            shutdown::enter(&mut scanout, &font);
        }
        supervisor.reap();
        supervisor.ensure_minimum();
        input.validate_drag(&windows, &mut damage);
        if windows.focused() != focus_before {
            damage.add(taskbar.strip_rect());
        }
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

fn dismiss_splash() {
    let pid = fs::read_to_string("/run/splash.pid")
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .and_then(Pid::new);
    if let Some(pid) = pid {
        let _ = process::signal(pid, Signal::Terminate);
    }
    let _ = fs::remove_file("/run/splash.pid");
}

fn listen_socket() -> Result<UnixListener, ()> {
    let _ = fs::remove_file(display_proto::SOCKET_PATH);
    let listener = UnixListener::bind(display_proto::SOCKET_PATH).map_err(|_| ())?;
    listener.set_nonblocking(true).map_err(|_| ())?;
    Ok(listener)
}

fn accept_clients(listener: &UnixListener, clients: &mut Clients) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => prepare_client(stream, clients),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn prepare_client(stream: UnixStream, clients: &mut Clients) {
    if stream.set_nonblocking(true).is_ok() {
        let _ = clients.add(stream);
    }
}
