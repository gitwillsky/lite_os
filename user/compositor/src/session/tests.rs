use super::*;

#[test]
fn app_records_only_its_first_presented_scene() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut app = App {
        stream,
        id: "music-player".to_owned(),
        configure: None,
        last_revision: 0,
        pending: None,
        current: None,
        first_scene_presented: false,
    };

    assert!(!app.first_scene_presented);
    app.first_scene_presented = true;
    assert!(app.first_scene_presented);
}

#[test]
fn app_first_presented_marker_is_one_complete_line() {
    assert_eq!(
        scene::app_first_scene_presented_marker(7),
        "compositor: app 7 first scene presented\n"
    );
}

#[test]
fn buffered_hangup_is_a_terminal_connection_event() {
    assert!(connection_closed(PollEvents::READ | PollEvents::HANGUP));
    assert!(connection_closed(PollEvents::ERROR));
    assert!(!connection_closed(PollEvents::READ));
}

#[test]
fn app_teardown_revokes_queued_paint_and_all_window_routes() {
    let mut paint = VecDeque::from([Owner::Desktop, Owner::App(7), Owner::App(8)]);
    let mut routing = vec![
        RoutingNode {
            surface_id: 0,
            window_group: 7,
            bounds: Rect::default(),
            input: Vec::new(),
        },
        RoutingNode {
            surface_id: 7,
            window_group: 7,
            bounds: Rect::default(),
            input: Vec::new(),
        },
        RoutingNode {
            surface_id: 8,
            window_group: 8,
            bounds: Rect::default(),
            input: Vec::new(),
        },
    ];

    revoke_surface_paint(&mut paint, 7);
    revoke_surface_routing(&mut routing, 7);

    assert!(paint == VecDeque::from([Owner::Desktop, Owner::App(8)]));
    assert_eq!(routing.len(), 1);
    assert_eq!(routing[0].surface_id, 8);
}
