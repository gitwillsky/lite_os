//! 外壳运行时探针：由 compositor 经 WAYLAND_SOCKET 启动，仅注入 disposable gate 镜像。
//! 1. 等普通窗口用例完成后映射背景与任务栏。2. 通过 foreign-toplevel 控制另一个客户端。
//! 3. 收到窗口 closed 后销毁图层、停止管理器并退出，验证连接与工作区清理。

#[path = "../probe_connection.rs"]
mod probe_connection;

use probe_connection::Connection;
use std::{fs, io, process::ExitCode, thread, time::Duration};
use wayland_wire::{
    protocol::{
        AnyEvent, Interface, wl_compositor, wl_display, wl_keyboard, wl_pointer, wl_registry,
        wl_seat, wl_surface, zwlr_foreign_toplevel_handle_v1 as handle,
        zwlr_foreign_toplevel_manager_v1 as manager, zwlr_layer_shell_v1 as shell,
        zwlr_layer_surface_v1 as layer,
    },
    wire::{DynamicNewId, EncodeError, ObjectId},
};

const ALL: u32 = 15;
const BOTTOM_LEFT_RIGHT: u32 = 2 | 4 | 8;
const BACKGROUND: u32 = 0xff183858;
const PANEL: u32 = 0xffc0c0c0;
const CURSOR: u32 = 0xffff00ff;

fn run() -> io::Result<()> {
    while !std::path::Path::new("/var/tmp/shell-start").exists() {
        thread::sleep(Duration::from_millis(20));
    }
    let mut connection = Connection::connect()?;
    let registry = connection.new_object(Interface::WlRegistry);
    let done = connection.new_object(Interface::WlCallback);
    wl_display::Request::GetRegistry { registry }
        .encode(ObjectId::DISPLAY, &mut connection.writer)
        .map_err(io::Error::other)?;
    wl_display::Request::Sync { callback: done }
        .encode(ObjectId::DISPLAY, &mut connection.writer)
        .map_err(io::Error::other)?;
    connection.flush()?;
    let mut globals = Vec::new();
    connection.wait_callback(done, |_, event| {
        if let AnyEvent::WlRegistry(wl_registry::Event::Global {
            name,
            interface,
            version,
        }) = event
        {
            globals.push((name, interface, version));
        }
    })?;
    let mut bind = |interface: Interface, wanted: u32| -> io::Result<ObjectId> {
        let (name, _, version) = globals
            .iter()
            .find(|(_, text, _)| text == interface.name())
            .ok_or_else(|| io::Error::other(format!("missing {}", interface.name())))?;
        let object = connection.new_object(interface);
        wl_registry::Request::Bind {
            name: *name,
            id: DynamicNewId {
                interface: interface.name().into(),
                version: wanted.min(*version),
                id: object,
            },
        }
        .encode(registry, &mut connection.writer)
        .map_err(io::Error::other)?;
        Ok(object)
    };
    let compositor = bind(Interface::WlCompositor, 6)?;
    let shm = bind(Interface::WlShm, 1)?;
    let seat = bind(Interface::WlSeat, 7)?;
    let layer_shell = bind(Interface::ZwlrLayerShellV1, 5)?;
    let manager = bind(Interface::ZwlrForeignToplevelManagerV1, 3)?;
    let pointer = connection.new_object(Interface::WlPointer);
    let keyboard = connection.new_object(Interface::WlKeyboard);
    let background = connection.new_object(Interface::WlSurface);
    let panel = connection.new_object(Interface::WlSurface);
    let cursor = connection.new_object(Interface::WlSurface);
    let background_role = connection.new_object(Interface::ZwlrLayerSurfaceV1);
    let panel_role = connection.new_object(Interface::ZwlrLayerSurfaceV1);
    (|| -> Result<(), EncodeError> {
        wl_seat::Request::GetPointer { id: pointer }.encode(seat, &mut connection.writer)?;
        wl_seat::Request::GetKeyboard { id: keyboard }.encode(seat, &mut connection.writer)?;
        for surface in [background, panel, cursor] {
            wl_compositor::Request::CreateSurface { id: surface }
                .encode(compositor, &mut connection.writer)?;
        }
        for (surface, role, level, anchor, height, zone, namespace) in [
            (
                background,
                background_role,
                shell::Layer::Background,
                ALL,
                0,
                -1,
                "probe-background",
            ),
            (
                panel,
                panel_role,
                shell::Layer::Top,
                BOTTOM_LEFT_RIGHT,
                28,
                28,
                "probe-panel",
            ),
        ] {
            shell::Request::GetLayerSurface {
                id: role,
                surface,
                output: None,
                layer: level,
                namespace: namespace.into(),
            }
            .encode(layer_shell, &mut connection.writer)?;
            layer::Request::SetAnchor { anchor }.encode(role, &mut connection.writer)?;
            layer::Request::SetSize { width: 0, height }.encode(role, &mut connection.writer)?;
            layer::Request::SetExclusiveZone { zone }.encode(role, &mut connection.writer)?;
            wl_surface::Request::Commit.encode(surface, &mut connection.writer)?;
        }
        Ok(())
    })()
    .map_err(io::Error::other)?;
    let cursor_frame = connection.new_object(Interface::WlCallback);
    connection.draw_solid(shm, cursor, (8, 8), CURSOR, 1, Some(cursor_frame))?;
    let mut configured = 0;
    let mut pending_frames = std::collections::HashMap::new();
    let mut target = None;
    let mut x = 0;
    let mut exclusive = false;
    let mut tall = false;
    loop {
        connection.flush()?;
        let (object, event) = connection.next_event()?;
        match event {
            AnyEvent::ZwlrLayerSurfaceV1(layer::Event::Configure {
                serial,
                width,
                height,
            }) => {
                layer::Request::AckConfigure { serial }
                    .encode(object, &mut connection.writer)
                    .map_err(io::Error::other)?;
                let (surface, color) = if object == panel_role {
                    (panel, PANEL)
                } else {
                    (background, BACKGROUND)
                };
                let frame = connection.new_object(Interface::WlCallback);
                connection.draw_solid(
                    shm,
                    surface,
                    (width as i32, height as i32),
                    color,
                    1,
                    Some(frame),
                )?;
                pending_frames.insert(frame, (width, height));
            }
            AnyEvent::WlCallback(_) => {
                if object == cursor_frame {
                    // 只有实际上传并参加呈现的硬件光标才推进 callback，不能把 set_cursor 请求当成功。
                    println!("shell-probe: cursor presented");
                }
                if let Some((width, height)) = pending_frames.remove(&object) {
                    configured += 1;
                    println!("shell-probe: configured {width}x{height}");
                    if configured == 2 {
                        fs::write("/var/tmp/shell-ready", b"ready")?;
                        println!("shell-probe: ready");
                    }
                }
            }
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { toplevel }) => {
                connection.adopt(toplevel, Interface::ZwlrForeignToplevelHandleV1);
                target = Some(toplevel);
                println!("shell-probe: toplevel");
            }
            AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Title { title }) => {
                println!("shell-probe: title {title}")
            }
            AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::AppId { app_id }) => {
                println!("shell-probe: app_id {app_id}")
            }
            AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::State { state }) => {
                let values: Vec<u32> = state
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|value| u32::from_ne_bytes(*value))
                    .collect();
                println!("shell-probe: state {values:?}");
            }
            AnyEvent::WlPointer(wl_pointer::Event::Enter {
                serial, surface_x, ..
            }) => {
                x = surface_x.to_f64() as i32;
                wl_pointer::Request::SetCursor {
                    serial,
                    surface: Some(cursor),
                    hotspot_x: 2,
                    hotspot_y: 3,
                }
                .encode(pointer, &mut connection.writer)
                .map_err(io::Error::other)?;
                println!("shell-probe: cursor requested");
            }
            AnyEvent::WlPointer(wl_pointer::Event::Motion { surface_x, .. }) => {
                x = surface_x.to_f64() as i32
            }
            AnyEvent::WlPointer(wl_pointer::Event::Button {
                state: wl_pointer::ButtonState::Pressed,
                ..
            }) => {
                let Some(target) = target else { continue };
                let request = match x / 100 {
                    0 => Some(handle::Request::SetMaximized),
                    1 => Some(handle::Request::SetMinimized),
                    2 => Some(handle::Request::Activate { seat }),
                    3 => Some(handle::Request::Close),
                    4 => {
                        tall = !tall;
                        let height = if tall { 48 } else { 28 };
                        layer::Request::SetSize { width: 0, height }
                            .encode(panel_role, &mut connection.writer)
                            .map_err(io::Error::other)?;
                        layer::Request::SetExclusiveZone {
                            zone: height as i32,
                        }
                        .encode(panel_role, &mut connection.writer)
                        .map_err(io::Error::other)?;
                        wl_surface::Request::Commit
                            .encode(panel, &mut connection.writer)
                            .map_err(io::Error::other)?;
                        None
                    }
                    5 => {
                        exclusive = !exclusive;
                        layer::Request::SetKeyboardInteractivity {
                            keyboard_interactivity: if exclusive {
                                layer::KeyboardInteractivity::Exclusive
                            } else {
                                layer::KeyboardInteractivity::None
                            },
                        }
                        .encode(panel_role, &mut connection.writer)
                        .map_err(io::Error::other)?;
                        wl_surface::Request::Commit
                            .encode(panel, &mut connection.writer)
                            .map_err(io::Error::other)?;
                        println!("shell-probe: exclusive {exclusive}");
                        None
                    }
                    7 => {
                        layer::Request::SetKeyboardInteractivity {
                            keyboard_interactivity: layer::KeyboardInteractivity::OnDemand,
                        }
                        .encode(panel_role, &mut connection.writer)
                        .map_err(io::Error::other)?;
                        wl_surface::Request::Commit
                            .encode(panel, &mut connection.writer)
                            .map_err(io::Error::other)?;
                        println!("shell-probe: on demand");
                        None
                    }
                    _ => None,
                };
                if let Some(request) = request {
                    request
                        .encode(target, &mut connection.writer)
                        .map_err(io::Error::other)?;
                    println!("shell-probe: action {}", x / 100);
                }
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { .. }) => {
                println!("shell-probe: keyboard enter")
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. }) => {
                println!("shell-probe: keyboard leave")
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. }) => {
                println!("shell-probe: key {key} {state:?}")
            }
            AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Closed) => {
                println!("shell-probe: closed");
                handle::Request::Destroy
                    .encode(object, &mut connection.writer)
                    .map_err(io::Error::other)?;
                // panel 可能已在前一个检查中销毁；连接退出会清理仍存活的所有 surface。
                manager::Request::Stop
                    .encode(manager, &mut connection.writer)
                    .map_err(io::Error::other)?;
            }
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Finished) => {
                println!("shell-probe: finished");
                fs::write("/var/tmp/shell-finished", b"finished")?;
                return Ok(());
            }
            _ => {}
        }
    }
}

fn main() -> ExitCode {
    match run() {
        // 探针连接已 drop；保持监督进程存活，使门禁能继续独立验证普通客户端。
        Ok(()) => loop {
            std::thread::park();
        },
        Err(error) => {
            eprintln!("shell-probe: error: {error}");
            ExitCode::FAILURE
        }
    }
}
