use super::*;

fn globals_of(mut harness: Harness) -> Vec<String> {
    harness
        .send(|w| {
            wl_display::Request::GetRegistry { registry: id(2) }
                .encode(ObjectId::DISPLAY, w)
                .unwrap()
        })
        .unwrap();
    harness
        .events(&[(2, Interface::WlRegistry)])
        .into_iter()
        .filter_map(|event| match event.event {
            AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }) => Some(interface),
            _ => None,
        })
        .collect()
}

fn bind(harness: &mut Harness, name: u32, interface: &str) -> Result<(), Disconnected> {
    harness.send(|w| {
        wl_display::Request::GetRegistry { registry: id(2) }
            .encode(ObjectId::DISPLAY, w)
            .unwrap()
    })?;
    harness.events(&[(2, Interface::WlRegistry)]);
    harness.send(|w| {
        wl_registry::Request::Bind {
            name,
            id: DynamicNewId {
                interface: interface.into(),
                version: 1,
                id: id(50),
            },
        }
        .encode(id(2), w)
        .unwrap()
    })
}

#[test]
fn ordinary_clients_see_the_window_list_but_never_the_shell_globals() {
    let globals = globals_of(Harness::new());
    assert!(globals.contains(&"zwlr_foreign_toplevel_manager_v1".to_owned()));
    assert!(
        !globals.iter().any(|name| name.contains("layer_shell")
            || name.contains("security_context")
            || name == "liteos_shell_v1"),
        "{globals:?}"
    );
}

#[test]
fn privileged_clients_see_layer_shell_and_foreign_toplevel() {
    let globals = globals_of(Harness {
        client: Client::session(OUTPUT),
    });
    assert!(globals.contains(&"zwlr_layer_shell_v1".to_owned()));
    assert!(globals.contains(&"zwlr_foreign_toplevel_manager_v1".to_owned()));
    assert!(globals.contains(&"wp_security_context_manager_v1".to_owned()));
    assert!(!globals.contains(&"liteos_shell_v1".to_owned()));
    assert_eq!(globals.len(), 14);
}

#[test]
fn an_ordinary_client_cannot_bind_a_shell_global_by_guessing_its_name() {
    for (name, interface) in [
        (6, "zwlr_layer_shell_v1"),
        (8, "wp_security_context_manager_v1"),
        (16, "liteos_shell_v1"),
    ] {
        let mut harness = Harness::new();
        assert!(bind(&mut harness, name, interface).is_err(), "{interface}");
        harness.expect_error(wl_display::Error::InvalidObject as u32);
    }
}

#[test]
fn a_delegated_shell_has_shell_globals_but_cannot_delegate_again() {
    let globals = globals_of(Harness {
        client: Client::shell(OUTPUT),
    });
    assert!(globals.contains(&"zwlr_layer_shell_v1".to_owned()));
    assert!(globals.contains(&"zwlr_foreign_toplevel_manager_v1".to_owned()));
    assert!(!globals.contains(&"wp_security_context_manager_v1".to_owned()));
    assert!(globals.contains(&"liteos_shell_v1".to_owned()));
    assert_eq!(globals.len(), 14);
}

#[test]
fn shell_control_lifetime_and_state_are_bound_to_the_authorized_object() {
    use wayland_wire::protocol::liteos_shell_v1 as shell;
    let mut harness = Harness {
        client: Client::shell(OUTPUT),
    };
    bind(&mut harness, 16, "liteos_shell_v1").unwrap();
    assert!(harness.client.has_shell_control());
    harness
        .client
        .sync_shell(shell::state::CANUNDO | shell::state::UNDOCASCADE);
    let events = harness.events(&[(50, Interface::LiteosShellV1)]);
    assert!(
        matches!(&events[..], [Event { object: 50, event: AnyEvent::LiteosShellV1(shell::Event::State { flags }) }] if *flags == shell::state::CANUNDO)
    );
    harness.client.sync_shell(shell::state::CANUNDO);
    assert!(harness.events(&[(50, Interface::LiteosShellV1)]).is_empty());
    harness
        .send(|w| shell::Request::Destroy.encode(id(50), w).unwrap())
        .unwrap();
    assert!(!harness.client.has_shell_control());
}

#[test]
fn a_v1_shell_binding_cannot_issue_v2_arrangements() {
    use wayland_wire::protocol::liteos_shell_v1 as shell;
    let mut harness = Harness {
        client: Client::shell(OUTPUT),
    };
    bind(&mut harness, 16, "liteos_shell_v1").unwrap();
    assert!(
        harness
            .send(|w| shell::Request::Arrange {
                layout: shell::Arrangement::Cascade,
            }
            .encode(id(50), w)
            .unwrap())
            .is_err()
    );
    harness.expect_error(wl_display::Error::InvalidMethod as u32);
    assert!(harness.client.take_requests().is_empty());
}
