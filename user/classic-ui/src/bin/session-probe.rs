//! 会话交接门禁：root 父客户端委派独立连接，标准 Command 降权后的子进程创建外壳图层。
//! 这里只验证权限机制与协议生命周期，不实现或绕过密码认证。

use classic_ui::{
    Display, EventKind, KeyboardInteractivity, Layer, LayerSettings, WindowId, layer_anchor,
};
use linux_uapi::process::{Identity, spawn_with_wayland_socket};
use std::{fs, io, path::Path, process::ExitCode};

const SOCKET: &str = "/run/liteos/session-probe-wayland";
const PARENT: u32 = 0xff202080;
const BACKGROUND: u32 = 0xff205020;
const PANEL: u32 = 0xffc0c0c0;

fn identity() -> io::Result<(Vec<u32>, Vec<u32>, Vec<u32>)> {
    let status = fs::read_to_string("/proc/self/status")?;
    let field = |name: &str| -> io::Result<Vec<u32>> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .ok_or_else(|| io::Error::other("missing process identity"))?
            .split_whitespace()
            .map(|value| value.parse().map_err(io::Error::other))
            .collect()
    };
    Ok((field("Uid:")?, field("Gid:")?, field("Groups:")?))
}

fn solid(display: &mut Display, window: WindowId, color: u32) -> io::Result<()> {
    let configuration = display
        .configuration(window)
        .ok_or_else(|| io::Error::other("missing configure"))?;
    let (width, height) = configuration.pixel_size()?;
    display.present(window, &vec![color; width as usize * height as usize])
}

fn parent() -> io::Result<()> {
    let (connected, _peer) = std::os::unix::net::UnixStream::pair()?;
    if linux_uapi::unix::unix_listener_from_fd(connected.into()).is_ok() {
        return Err(io::Error::other(
            "connected socket passed listener validation",
        ));
    }
    println!("session-probe: invalid listener denied");
    let mut display = Display::connect()?;
    let window = display.create_layer(
        "session-parent",
        LayerSettings {
            layer: Layer::Overlay,
            keyboard: KeyboardInteractivity::Exclusive,
            exclusive_zone: -1,
            ..LayerSettings::default()
        },
    )?;
    loop {
        let event = display.next_event()?;
        match event.kind {
            EventKind::Configure(_) if display.ready_to_draw(window) => {
                solid(&mut display, window, PARENT)?
            }
            EventKind::FramePresented { .. } => println!("session-probe: parent ready"),
            EventKind::Key(key) if key.pressed && key.code == 32 => break, // D：探针的交接触发，不是认证结果。
            _ => {}
        }
    }
    let connection = display.delegate_shell(Path::new(SOCKET), "runtime-probe")?;
    if Path::new(SOCKET).exists() {
        return Err(io::Error::other(
            "delegated socket pathname remains visible",
        ));
    }
    let (socket, lease) = connection.split();
    display.destroy_window(window)?;
    drop(display);
    // 父 Wayland 客户端已断开；listener 与之后启动的子客户端不得随它消失。
    let mut child = spawn_with_wayland_socket(
        Path::new("/bin/session-probe").as_os_str(),
        socket.into(),
        Some(Identity {
            uid: 65534,
            gid: 65534,
        }),
    )?;
    println!("session-probe: delegated after creator disconnect");
    let status = child.wait()?;
    drop(lease);
    if !status.success() {
        return Err(io::Error::other(format!("desktop child failed: {status}")));
    }
    println!("session-probe: finished");
    Ok(())
}

fn desktop() -> io::Result<()> {
    let (uids, gids, groups) = identity()?;
    if uids != [65534; 4] || gids != [65534; 4] || !groups.is_empty() {
        return Err(io::Error::other(format!(
            "unexpected child identity: {uids:?} {gids:?} {groups:?}"
        )));
    }
    println!("session-probe: identity uid=gid=65534 groups=empty");
    let mut display = Display::connect()?;
    let nested = match display.delegate_shell(Path::new(SOCKET), "nested") {
        Ok(_) => return Err(io::Error::other("nested delegation was allowed")),
        Err(error) => error,
    };
    if nested.kind() != io::ErrorKind::PermissionDenied {
        return Err(nested);
    }
    println!("session-probe: nested denied");
    let background = display.create_layer(
        "delegated-background",
        LayerSettings {
            layer: Layer::Background,
            keyboard: KeyboardInteractivity::OnDemand,
            exclusive_zone: -1,
            ..LayerSettings::default()
        },
    )?;
    let mut settings = LayerSettings {
        layer: Layer::Top,
        width: 0,
        height: 28,
        anchor: layer_anchor::BOTTOM | layer_anchor::LEFT | layer_anchor::RIGHT,
        margin: [0; 4],
        exclusive_zone: 28,
        keyboard: KeyboardInteractivity::OnDemand,
    };
    let panel = display.create_layer("delegated-panel", settings)?;
    let mut presented = std::collections::HashSet::new();
    loop {
        let event = display.next_event()?;
        match event.kind {
            EventKind::Configure(configuration) => {
                if display.ready_to_draw(event.window) {
                    solid(
                        &mut display,
                        event.window,
                        if event.window == panel {
                            PANEL
                        } else {
                            BACKGROUND
                        },
                    )?;
                }
                println!(
                    "session-probe: configure {}x{}",
                    configuration.width, configuration.height
                );
            }
            EventKind::FramePresented { .. } => {
                presented.insert(event.window);
                if presented.len() == 2 {
                    println!("session-probe: desktop ready");
                }
            }
            EventKind::Focus(focused) => println!("session-probe: focus {focused}"),
            EventKind::Key(key) if key.pressed => match key.code {
                50 => {
                    // M：任务栏高度改变，已有窗口的 configure/frame 路径继续生效。
                    settings.height = 48;
                    settings.exclusive_zone = 48;
                    display.configure_layer(panel, settings)?;
                }
                35 => {
                    display.unmap_layer(panel)?;
                    println!("session-probe: panel unmapped");
                } // H
                19 => {
                    display.configure_layer(panel, settings)?;
                    println!("session-probe: panel remap requested");
                } // R
                16 => {
                    // Q
                    display.destroy_window(panel)?;
                    display.destroy_window(background)?;
                    println!("session-probe: desktop closed");
                    return Ok(());
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn main() -> ExitCode {
    let is_parent = identity().map(|(uid, _, _)| uid == [0; 4]);
    let result =
        is_parent.and_then(|parent_process| if parent_process { parent() } else { desktop() });
    match result {
        Ok(()) if identity().is_ok_and(|(uid, _, _)| uid == [0; 4]) => {
            // 父协议连接和租约已释放；监督进程继续存活，避免触发产品的 logon 退出重建策略。
            loop {
                std::thread::park();
            }
        }
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("session-probe: error: {error}");
            ExitCode::FAILURE
        }
    }
}
