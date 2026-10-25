use wayland_wire::protocol::{
    zwlr_foreign_toplevel_handle_v1 as handle, zwlr_foreign_toplevel_manager_v1 as manager,
};

use super::*;
use crate::wayland::{ClientId, ForeignAction, ForeignInfo, ForeignRequest, ForeignState};

const MANAGER: [(u32, Interface); 1] = [(52, Interface::ZwlrForeignToplevelManagerV1)];

fn window(client: u64, title: &str, state: ForeignState) -> ForeignInfo {
    ForeignInfo {
        client: ClientId(client),
        surface: id(5),
        title: title.into(),
        app_id: "app".into(),
        state,
    }
}

/// 取走事件，返回 `(对象 id, 事件)`。
fn events(harness: &mut Harness) -> Vec<(u32, AnyEvent)> {
    harness
        .events(&MANAGER)
        .into_iter()
        .map(|event| (event.object, event.event))
        .collect()
}

fn handle_of(events: &[(u32, AnyEvent)]) -> u32 {
    events
        .iter()
        .find_map(|(_, event)| match event {
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { toplevel }) => {
                Some(toplevel.get())
            }
            _ => None,
        })
        .expect("a toplevel event")
}

#[test]
fn a_new_window_is_announced_with_title_app_id_state_and_done_in_order() {
    let mut harness = Harness::privileged();
    let state = ForeignState {
        activated: true,
        maximized: true,
        ..ForeignState::default()
    };
    harness.client.sync_foreign(&[window(2, "记事本", state)]);
    let events = events(&mut harness);
    let handle = handle_of(&events);
    assert!(handle >= 0xFF00_0000, "server-allocated id");
    assert!(
        matches!(&events[1], (id, AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Title { title })) if *id == handle && title == "记事本")
    );
    assert!(
        matches!(&events[2].1, AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::AppId { app_id }) if app_id == "app")
    );
    match &events[3].1 {
        AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::State { state }) => {
            let values: Vec<u32> = state
                .chunks(4)
                .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                .collect();
            assert_eq!(values, [0, 2], "maximized, activated");
        }
        _ => panic!("expected state"),
    }
    assert!(matches!(
        events[4].1,
        AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Done)
    ));
    assert_eq!(events.len(), 5);
}

#[test]
fn syncing_an_unchanged_list_sends_nothing() {
    let mut harness = Harness::privileged();
    let list = [window(2, "a", ForeignState::default())];
    harness.client.sync_foreign(&list);
    events(&mut harness);
    harness.client.sync_foreign(&list);
    assert!(events(&mut harness).is_empty());
}

#[test]
fn only_the_changed_properties_are_updated_and_each_update_ends_with_done() {
    let mut harness = Harness::privileged();
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    events(&mut harness);
    let minimized = ForeignState {
        minimized: true,
        ..ForeignState::default()
    };
    harness.client.sync_foreign(&[window(2, "a", minimized)]);
    let events_after_state = events(&mut harness);
    assert_eq!(events_after_state.len(), 2);
    assert!(matches!(
        events_after_state[0].1,
        AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::State { .. })
    ));
    assert!(matches!(
        events_after_state[1].1,
        AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Done)
    ));
    harness.client.sync_foreign(&[window(2, "b", minimized)]);
    let events_after_title = events(&mut harness);
    assert!(
        matches!(&events_after_title[0].1, AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Title { title }) if title == "b")
    );
    assert_eq!(events_after_title.len(), 2);
}

#[test]
fn a_vanished_window_is_closed_and_a_returning_one_gets_a_fresh_handle() {
    let mut harness = Harness::privileged();
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    let first = handle_of(&events(&mut harness));
    harness.client.sync_foreign(&[]);
    let closed = events(&mut harness);
    assert!(
        matches!(&closed[..], [(id, AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Closed))] if *id == first)
    );
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    assert_ne!(handle_of(&events(&mut harness)), first);
}

#[test]
fn handle_requests_become_actions_on_the_right_window() {
    let mut harness = Harness::privileged();
    harness.client.sync_foreign(&[
        window(2, "a", ForeignState::default()),
        window(3, "b", ForeignState::default()),
    ]);
    let announced = events(&mut harness);
    let handles: Vec<u32> = announced
        .iter()
        .filter_map(|(_, event)| match event {
            AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { toplevel }) => {
                Some(toplevel.get())
            }
            _ => None,
        })
        .collect();
    assert_eq!(handles.len(), 2);
    let send = |harness: &mut Harness, target: u32, request: handle::Request| {
        harness
            .send(|w| request.encode(id(target), w).unwrap())
            .unwrap();
    };
    send(
        &mut harness,
        handles[1],
        handle::Request::Activate { seat: id(40) },
    );
    send(&mut harness, handles[0], handle::Request::SetMinimized);
    send(&mut harness, handles[0], handle::Request::UnsetMinimized);
    send(&mut harness, handles[1], handle::Request::SetMaximized);
    send(&mut harness, handles[1], handle::Request::UnsetFullscreen);
    send(&mut harness, handles[0], handle::Request::Close);
    send(
        &mut harness,
        handles[0],
        handle::Request::SetRectangle {
            surface: id(5),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
    );
    let requests = harness.client.take_foreign_requests();
    let request = |client, action| ForeignRequest {
        shell: ClientId(0),
        client: ClientId(client),
        surface: id(5),
        action,
    };
    assert_eq!(
        requests,
        [
            request(3, ForeignAction::Activate),
            request(2, ForeignAction::Minimize(true)),
            request(2, ForeignAction::Minimize(false)),
            request(3, ForeignAction::Maximize(true)),
            request(3, ForeignAction::Fullscreen(false)),
            request(2, ForeignAction::Close),
        ],
        "set_rectangle is accepted and ignored"
    );
}

#[test]
fn stop_finishes_only_the_manager_and_keeps_live_handles() {
    let mut harness = Harness::privileged();
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    let handle_id = handle_of(&events(&mut harness));
    harness
        .send(|w| manager::Request::Stop.encode(id(52), w).unwrap())
        .unwrap();
    let after = events(&mut harness);
    assert!(matches!(
        after[0].1,
        AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Finished)
    ));
    assert_eq!(
        after.len(),
        2,
        "finished then delete_id for the manager, no closed"
    );
    harness.client.sync_foreign(&[
        window(2, "renamed", ForeignState::default()),
        window(3, "new", ForeignState::default()),
    ]);
    let updates = events(&mut harness);
    assert!(updates.iter().all(|(object, _)| *object == handle_id));
    harness.client.sync_foreign(&[]);
    assert!(matches!(
        events(&mut harness)[0].1,
        AnyEvent::ZwlrForeignToplevelHandleV1(handle::Event::Closed)
    ));
    assert!(!harness.client.has_foreign_manager());
}

#[test]
fn destroying_a_handle_sends_no_delete_id_for_a_server_allocated_object() {
    let mut harness = Harness::privileged();
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    let handle_id = handle_of(&events(&mut harness));
    harness
        .send(|w| handle::Request::Destroy.encode(id(handle_id), w).unwrap())
        .unwrap();
    assert!(
        events(&mut harness).is_empty(),
        "libwayland would reject delete_id for a server id"
    );
    // 句柄销毁后窗口的变化不再发给它。
    harness
        .client
        .sync_foreign(&[window(2, "renamed", ForeignState::default())]);
    let after = events(&mut harness);
    assert!(
        after.is_empty(),
        "destroying a handle must not reannounce the same window"
    );
}

#[test]
fn managers_can_be_bound_independently_and_rebound_after_stop() {
    let mut harness = Harness::privileged();
    let bind = |harness: &mut Harness| {
        harness
            .send(|w| {
                wl_registry::Request::Bind {
                    name: 7,
                    id: DynamicNewId {
                        interface: "zwlr_foreign_toplevel_manager_v1".into(),
                        version: 3,
                        id: id(53),
                    },
                }
                .encode(id(2), w)
                .unwrap()
            })
            .unwrap()
    };
    bind(&mut harness);
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    let announced = harness.events(&[
        (52, Interface::ZwlrForeignToplevelManagerV1),
        (53, Interface::ZwlrForeignToplevelManagerV1),
    ]);
    assert_eq!(
        announced
            .iter()
            .filter(|event| matches!(
                event.event,
                AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { .. })
            ))
            .count(),
        2
    );
    harness
        .send(|w| manager::Request::Stop.encode(id(53), w).unwrap())
        .unwrap();
    harness.events(&[(53, Interface::ZwlrForeignToplevelManagerV1)]);
    bind(&mut harness);
    harness
        .client
        .sync_foreign(&[window(2, "a", ForeignState::default())]);
    let announced = harness.events(&[(53, Interface::ZwlrForeignToplevelManagerV1)]);
    assert_eq!(
        announced
            .iter()
            .filter(|event| matches!(
                event.event,
                AnyEvent::ZwlrForeignToplevelManagerV1(manager::Event::Toplevel { .. })
            ))
            .count(),
        1
    );
}

#[test]
fn a_closed_handle_cannot_control_a_new_window_reusing_the_surface_id() {
    let mut harness = Harness::privileged();
    harness
        .client
        .sync_foreign(&[window(2, "old", ForeignState::default())]);
    let old = handle_of(&events(&mut harness));
    harness.client.sync_foreign(&[]);
    events(&mut harness);
    harness
        .client
        .sync_foreign(&[window(2, "new", ForeignState::default())]);
    assert_ne!(old, handle_of(&events(&mut harness)));
    harness
        .send(|w| handle::Request::Close.encode(id(old), w).unwrap())
        .unwrap();
    assert!(harness.client.take_foreign_requests().is_empty());
    harness
        .send(|w| handle::Request::Destroy.encode(id(old), w).unwrap())
        .unwrap();
    assert!(events(&mut harness).is_empty());
}
