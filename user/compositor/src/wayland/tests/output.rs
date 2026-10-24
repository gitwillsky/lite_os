use super::*;
use wayland_wire::protocol::wl_output;

/// 绑定 `wl_output`（id 30）并取走事件。
fn bind_output(harness: &mut Harness, version: u32) -> Vec<wl_output::Event> {
    harness
        .send(|w| {
            wl_registry::Request::Bind {
                name: 4,
                id: DynamicNewId {
                    interface: "wl_output".into(),
                    version,
                    id: id(30),
                },
            }
            .encode(id(2), w)
            .unwrap()
        })
        .unwrap();
    harness
        .events(&[(30, Interface::WlOutput)])
        .into_iter()
        .map(|event| match event.event {
            AnyEvent::WlOutput(event) => event,
            _ => panic!("unexpected event"),
        })
        .collect()
}

#[test]
fn binding_announces_the_mode_scale_and_names_up_to_done() {
    let mut harness = Harness::bound();
    let events = bind_output(&mut harness, 4);
    assert!(matches!(
        events[0],
        wl_output::Event::Geometry { x: 0, y: 0, .. }
    ));
    assert!(matches!(
        events[1],
        wl_output::Event::Mode {
            flags: 3,
            width: 1280,
            height: 800,
            refresh: 60_000
        }
    ));
    assert!(matches!(events[2], wl_output::Event::Scale { factor: 2 }));
    assert!(matches!(&events[3], wl_output::Event::Name { name } if name == "LITEOS-1"));
    assert!(matches!(events[4], wl_output::Event::Description { .. }));
    assert!(matches!(events[5], wl_output::Event::Done));
    assert_eq!(events.len(), 6);
}

#[test]
fn an_old_binding_gets_only_the_events_its_version_has() {
    let mut harness = Harness::bound();
    let events = bind_output(&mut harness, 1);
    assert_eq!(events.len(), 2, "geometry and mode only at version 1");
    let mut harness = Harness::bound();
    assert_eq!(
        bind_output(&mut harness, 3).len(),
        4,
        "no name or description before version 4"
    );
}

#[test]
fn surfaces_learn_the_preferred_scale_and_enter_the_output_once_they_have_content() {
    let mut harness = Harness::bound();
    bind_output(&mut harness, 4);
    harness
        .send(|w| {
            wl_compositor::Request::CreateSurface { id: id(5) }
                .encode(id(3), w)
                .unwrap()
        })
        .unwrap();
    let events = harness.events(&[(5, Interface::WlSurface)]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlSurface(wl_surface::Event::PreferredBufferScale { factor: 2 })
    ));
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
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    let events = harness.events(&[(5, Interface::WlSurface), (11, Interface::WlBuffer)]);
    assert!(events.iter().any(|event| matches!(
        event.event,
        AnyEvent::WlSurface(wl_surface::Event::Enter { output }) if output == id(30)
    )));
    // 第二次 commit 不再重复 enter。
    harness
        .send(|w| wl_surface::Request::Commit.encode(id(5), w).unwrap())
        .unwrap();
    assert!(harness.events(&[(5, Interface::WlSurface)]).is_empty());
}

#[test]
fn release_destroys_the_output_object() {
    let mut harness = Harness::bound();
    bind_output(&mut harness, 4);
    harness
        .send(|w| wl_output::Request::Release.encode(id(30), w).unwrap())
        .unwrap();
    let events = harness.events(&[]);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlDisplay(wl_display::Event::DeleteId { id: 30 })
    ));
}
