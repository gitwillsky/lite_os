use super::*;
use crate::wayland::ToplevelState;

const XDG_OBJECTS: [(u32, Interface); 2] =
    [(21, Interface::XdgSurface), (22, Interface::XdgToplevel)];

/// 建立窗口：surface 5、xdg_surface 21、toplevel 22，并发出初始 commit；返回初始 configure 的序号。
fn window(harness: &mut Harness) -> u32 {
    harness.create_surface(5);
    harness
        .send(|w| {
            xdg_wm_base::Request::GetXdgSurface {
                id: id(21),
                surface: id(5),
            }
            .encode(id(20), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            xdg_surface::Request::GetToplevel { id: id(22) }
                .encode(id(21), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let events = harness.events(&XDG_OBJECTS);
    assert!(matches!(
        events[0].event,
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
            width: 0,
            height: 0,
            ..
        })
    ));
    match events[1].event {
        AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => serial,
        _ => panic!("expected xdg_surface.configure"),
    }
}

fn ack(harness: &mut Harness, serial: u32) -> Result<(), Disconnected> {
    harness.send(|w| {
        xdg_surface::Request::AckConfigure { serial }
            .encode(id(21), w)
            .unwrap()
    })
}

/// 以 buffer 11 映射窗口（2×2 像素）。
fn map(harness: &mut Harness) {
    harness
        .create_buffer(&[0x7F; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
}

#[test]
fn the_first_commit_configures_and_an_acknowledged_window_maps() {
    let mut harness = Harness::bound();
    let serial = window(&mut harness);
    assert_eq!(harness.client.windows().count(), 0);
    ack(&mut harness, serial).unwrap();
    harness
        .send(|w| {
            xdg_toplevel::Request::SetTitle {
                title: "登录到 LiteOS".into(),
            }
            .encode(id(22), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            xdg_toplevel::Request::SetAppId {
                app_id: "logon".into(),
            }
            .encode(id(22), w)
            .unwrap()
        })
        .unwrap();
    map(&mut harness);
    let windows: Vec<_> = harness.client.windows().collect();
    assert_eq!(windows.len(), 1);
    assert_eq!(
        (windows[0].title, windows[0].app_id, windows[0].surface),
        ("登录到 LiteOS", "logon", id(5))
    );
    assert_eq!(
        (windows[0].content.width, windows[0].content.size),
        (2, (2, 2))
    );
}

#[test]
fn the_initial_configure_is_sent_exactly_once() {
    let mut harness = Harness::bound();
    let serial = window(&mut harness);
    ack(&mut harness, serial).unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    assert!(harness.events(&XDG_OBJECTS).is_empty());
}

#[test]
fn a_buffer_before_the_first_ack_is_unconfigured_buffer() {
    let mut harness = Harness::bound();
    window(&mut harness);
    harness
        .create_buffer(&[0; 16], 2, 2, 8, wl_shm::Format::Argb8888)
        .unwrap();
    harness
        .send(|w| {
            wl_surface::Request::Attach {
                buffer: Some(id(11)),
                x: 0,
                y: 0,
            }
            .encode(id(5), w)
            .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
            .is_err()
    );
    harness.expect_error(xdg_surface::Error::UnconfiguredBuffer as u32);
}

#[test]
fn acknowledging_an_unknown_serial_is_rejected_and_a_later_ack_covers_earlier_ones() {
    let mut harness = Harness::bound();
    let first = window(&mut harness);
    assert!(ack(&mut harness, first + 100).is_err());
    harness.expect_error(xdg_surface::Error::InvalidSerial as u32);

    let mut harness = Harness::bound();
    let first = window(&mut harness);
    harness.client.configure(
        id(5),
        300,
        200,
        ToplevelState {
            activated: true,
            ..ToplevelState::default()
        },
    );
    let events = harness.events(&XDG_OBJECTS);
    let (width, height, states) = match &events[0].event {
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        }) => (*width, *height, states.clone()),
        _ => panic!("expected toplevel configure"),
    };
    assert_eq!((width, height, states), (300, 200, vec![4, 0, 0, 0]));
    let second = match events[1].event {
        AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }) => serial,
        _ => panic!("expected xdg_surface.configure"),
    };
    assert_ne!(first, second);
    ack(&mut harness, second).unwrap();
    // 第一个序号已被后一个确认覆盖，不能再单独确认。
    assert!(ack(&mut harness, first).is_err());
}

#[test]
fn role_misuse_is_reported_with_the_documented_codes() {
    let mut harness = Harness::bound();
    window(&mut harness);
    assert!(
        harness
            .send(|w| {
                xdg_surface::Request::GetToplevel { id: id(23) }
                    .encode(id(21), w)
                    .unwrap()
            })
            .is_err()
    );
    harness.expect_error(xdg_surface::Error::AlreadyConstructed as u32);

    let mut harness = Harness::bound();
    window(&mut harness);
    assert!(
        harness
            .send(|w| {
                xdg_wm_base::Request::GetXdgSurface {
                    id: id(23),
                    surface: id(5),
                }
                .encode(id(20), w)
                .unwrap()
            })
            .is_err()
    );
    harness.expect_error(xdg_wm_base::Error::Role as u32);

    let mut harness = Harness::bound();
    window(&mut harness);
    assert!(
        harness
            .send(|w| xdg_surface::Request::Destroy.encode(id(21), w).unwrap())
            .is_err()
    );
    harness.expect_error(xdg_surface::Error::DefunctRoleObject as u32);

    let mut harness = Harness::bound();
    window(&mut harness);
    assert!(
        harness
            .send(|w| xdg_wm_base::Request::Destroy.encode(id(20), w).unwrap())
            .is_err()
    );
    harness.expect_error(xdg_wm_base::Error::DefunctSurfaces as u32);
}

#[test]
fn popup_with_incomplete_positioner_is_a_protocol_error() {
    let mut harness = Harness::bound();
    window(&mut harness);
    harness
        .send(|w| {
            xdg_wm_base::Request::CreatePositioner { id: id(30) }
                .encode(id(20), w)
                .unwrap()
        })
        .unwrap();
    assert!(
        harness
            .send(|w| {
                xdg_surface::Request::GetPopup {
                    id: id(31),
                    parent: Some(id(21)),
                    positioner: id(30),
                }
                .encode(id(21), w)
                .unwrap()
            })
            .is_err()
    );
    let message = harness.expect_error(xdg_wm_base::Error::InvalidPositioner as u32);
    assert!(message.contains("positioner"), "{message}");
}

#[test]
fn window_requests_are_queued_keyed_by_surface() {
    let mut harness = Harness::bound_with(
        Harness {
            client: Client::shell(OUTPUT),
        },
        3,
    );
    let serial = window(&mut harness);
    ack(&mut harness, serial).unwrap();
    for request in [
        xdg_toplevel::Request::Move {
            seat: id(40),
            serial: 1,
        },
        xdg_toplevel::Request::Resize {
            seat: id(40),
            serial: 1,
            edges: xdg_toplevel::ResizeEdge::BottomRight,
        },
        xdg_toplevel::Request::SetMaximized,
        xdg_toplevel::Request::UnsetMaximized,
        xdg_toplevel::Request::SetMinimized,
        xdg_toplevel::Request::SetFullscreen { output: None },
    ] {
        harness
            .send(|w| request.encode(id(22), w).unwrap())
            .unwrap();
    }
    let requests = harness.client.take_requests();
    let expected = [
        WindowRequest::Move { serial: 1 },
        WindowRequest::Resize {
            serial: 1,
            edges: xdg_toplevel::ResizeEdge::BottomRight as u32,
        },
        WindowRequest::Maximize(true),
        WindowRequest::Maximize(false),
        WindowRequest::Minimize,
        WindowRequest::Fullscreen(true),
    ];
    assert_eq!(
        requests,
        expected
            .map(|request| crate::wayland::WindowCommand::Toplevel(id(5), request))
            .to_vec()
    );
    assert!(harness.client.take_requests().is_empty());
    // 标准单窗命令、foreign-toplevel 与系统批量命令必须保留同一连接上的线序。
    use crate::wayland::{
        ClientId, ForeignAction, ForeignInfo, ForeignRequest, ForeignState, WindowCommand,
    };
    use wayland_wire::protocol::{
        liteos_shell_v1 as shell, zwlr_foreign_toplevel_handle_v1 as handle,
    };
    harness
        .send(|w| {
            for (name, interface, object, version) in [
                (7, "zwlr_foreign_toplevel_manager_v1", 52, 3),
                (16, "liteos_shell_v1", 60, 2),
            ] {
                wl_registry::Request::Bind {
                    name,
                    id: DynamicNewId {
                        interface: interface.into(),
                        version,
                        id: id(object),
                    },
                }
                .encode(id(2), w)
                .unwrap();
            }
        })
        .unwrap();
    harness.client.sync_foreign(&[ForeignInfo {
        client: ClientId(99),
        surface: id(5),
        title: "target".into(),
        app_id: "target".into(),
        state: ForeignState::default(),
    }]);
    let events = harness.events(&[(52, Interface::ZwlrForeignToplevelManagerV1)]);
    let target = events
        .iter()
        .find_map(|event| match event.event {
            AnyEvent::ZwlrForeignToplevelManagerV1(
                wayland_wire::protocol::zwlr_foreign_toplevel_manager_v1::Event::Toplevel {
                    toplevel,
                },
            ) => Some(toplevel),
            _ => None,
        })
        .unwrap();
    harness
        .send(|w| {
            xdg_toplevel::Request::SetMinimized
                .encode(id(22), w)
                .unwrap();
            handle::Request::SetMinimized.encode(target, w).unwrap();
            shell::Request::Perform {
                action: shell::Action::MinimizeAll,
            }
            .encode(id(60), w)
            .unwrap();
            shell::Request::Arrange {
                layout: shell::Arrangement::Vertical,
            }
            .encode(id(60), w)
            .unwrap();
            handle::Request::UnsetMinimized.encode(target, w).unwrap();
            shell::Request::UndoArrange.encode(id(60), w).unwrap();
            shell::Request::Perform {
                action: shell::Action::UndoMinimizeAll,
            }
            .encode(id(60), w)
            .unwrap();
        })
        .unwrap();
    let foreign = |action| {
        WindowCommand::Foreign(ForeignRequest {
            shell: ClientId(0),
            client: ClientId(99),
            surface: id(5),
            action,
        })
    };
    assert_eq!(
        harness.client.take_requests(),
        [
            WindowCommand::Toplevel(id(5), WindowRequest::Minimize),
            foreign(ForeignAction::Minimize(true)),
            WindowCommand::Shell(shell::Action::MinimizeAll),
            WindowCommand::Arrange(shell::Arrangement::Vertical),
            foreign(ForeignAction::Minimize(false)),
            WindowCommand::UndoArrange,
            WindowCommand::Shell(shell::Action::UndoMinimizeAll),
        ]
    );
    assert!(
        harness
            .send(|w| {
                xdg_toplevel::Request::SetMinSize {
                    width: -1,
                    height: 0,
                }
                .encode(id(22), w)
                .unwrap()
            })
            .is_err()
    );
    harness.expect_error(xdg_toplevel::Error::InvalidSize as u32);
}

#[test]
fn close_is_delivered_to_the_toplevel() {
    let mut harness = Harness::bound();
    window(&mut harness);
    harness.client.close(id(5));
    let events = harness.events(&XDG_OBJECTS);
    assert!(matches!(
        events[0].event,
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Close)
    ));
    // 没有角色的 surface 上关闭无事发生。
    harness.client.close(id(99));
    assert!(harness.events(&XDG_OBJECTS).is_empty());
}

#[test]
fn destroying_the_role_unmaps_and_a_new_role_must_be_configured_again() {
    let mut harness = Harness::bound();
    let serial = window(&mut harness);
    ack(&mut harness, serial).unwrap();
    map(&mut harness);
    assert_eq!(harness.client.windows().count(), 1);
    harness
        .send(|w| xdg_toplevel::Request::Destroy.encode(id(22), w).unwrap())
        .unwrap();
    assert_eq!(harness.client.windows().count(), 0);
    harness.events(&[(11, Interface::WlBuffer)]);
    harness
        .send(|w| {
            xdg_surface::Request::GetToplevel { id: id(22) }
                .encode(id(21), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let events = harness.events(&XDG_OBJECTS);
    assert!(
        events.iter().any(|event| matches!(
            event.event,
            AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { .. })
        )),
        "a recreated toplevel gets a fresh initial configure"
    );
}

#[test]
fn configure_states_are_encoded_in_protocol_order() {
    let mut harness = Harness::bound();
    window(&mut harness);
    let all = ToplevelState {
        activated: true,
        maximized: true,
        fullscreen: true,
        resizing: true,
        suspended: true,
    };
    harness.client.configure(id(5), 1280, 800, all);
    harness
        .client
        .configure(id(5), 0, 0, ToplevelState::default());
    let events = harness.events(&XDG_OBJECTS);
    match &events[0].event {
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure {
            width: 1280,
            height: 800,
            states,
        }) => {
            let values: Vec<u32> = states
                .chunks(4)
                .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                .collect();
            assert_eq!(
                values,
                [1, 2, 3, 4],
                "maximized, fullscreen, resizing, activated; a v3 toplevel never sees suspended (v6)"
            );
        }
        _ => panic!("expected a toplevel configure"),
    }
    assert!(
        matches!(&events[2].event, AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { states, .. }) if states.is_empty())
    );
}

#[test]
fn v6_toplevel_gets_capabilities_before_first_configure_and_suspended_state() {
    let mut harness = Harness::bound_xdg(6);
    harness.create_surface(5);
    harness
        .send(|w| {
            xdg_wm_base::Request::GetXdgSurface {
                id: id(21),
                surface: id(5),
            }
            .encode(id(20), w)
            .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            xdg_surface::Request::GetToplevel { id: id(22) }
                .encode(id(21), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let events = harness.events(&XDG_OBJECTS);
    let words = |bytes: &[u8]| -> Vec<u32> {
        bytes
            .chunks(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
            .collect()
    };
    match &events[0].event {
        AnyEvent::XdgToplevel(xdg_toplevel::Event::WmCapabilities { capabilities }) => {
            assert_eq!(words(capabilities), [1, 2, 3, 4]);
        }
        _ => panic!("v5+ requires wm_capabilities before the first configure"),
    }
    assert!(matches!(
        events[1].event,
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { .. })
    ));
    harness.client.configure(
        id(5),
        0,
        0,
        ToplevelState {
            suspended: true,
            ..ToplevelState::default()
        },
    );
    let events = harness.events(&XDG_OBJECTS);
    match &events[0].event {
        AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { states, .. }) => {
            assert_eq!(words(states), [9], "suspended");
        }
        _ => panic!("expected a toplevel configure"),
    }
}

#[test]
fn size_limits_are_committed_together_before_window_arrangement_can_use_them() {
    let mut harness = Harness::bound();
    let serial = window(&mut harness);
    ack(&mut harness, serial).unwrap();
    map(&mut harness);
    harness.events(&[(11, Interface::WlBuffer)]);
    harness
        .send(|w| {
            xdg_toplevel::Request::SetMinSize {
                width: 300,
                height: 200,
            }
            .encode(id(22), w)
            .unwrap();
            xdg_toplevel::Request::SetMaxSize {
                width: 500,
                height: 400,
            }
            .encode(id(22), w)
            .unwrap();
        })
        .unwrap();
    let window = harness.client.windows().next().unwrap();
    assert_eq!((window.min_size, window.max_size), ((0, 0), (0, 0)));
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let window = harness.client.windows().next().unwrap();
    assert_eq!((window.min_size, window.max_size), ((300, 200), (500, 400)));
    // 同一提交可以同时改变两端；约束之间的瞬时不一致不能使合法的最终状态失败。
    harness
        .send(|w| {
            xdg_toplevel::Request::SetMinSize {
                width: 600,
                height: 200,
            }
            .encode(id(22), w)
            .unwrap();
            xdg_toplevel::Request::SetMaxSize {
                width: 700,
                height: 400,
            }
            .encode(id(22), w)
            .unwrap();
            wl_surface::Request::Commit.encode(id(5), w).unwrap();
        })
        .unwrap();
    assert_eq!(
        harness.client.windows().next().unwrap().min_size,
        (600, 200)
    );
    assert!(
        harness
            .send(|w| {
                xdg_toplevel::Request::SetMaxSize {
                    width: 500,
                    height: 400,
                }
                .encode(id(22), w)
                .unwrap();
                wl_surface::Request::Commit.encode(id(5), w).unwrap();
            })
            .is_err()
    );
    harness.expect_error(xdg_toplevel::Error::InvalidSize as u32);
}
