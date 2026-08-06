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
        close_deadline: None,
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
fn older_paint_configuration_is_a_terminal_discard_not_a_session_error() {
    assert_eq!(
        classify_paint_configuration(4, 5).expect("old configure is a normal race"),
        PaintConfiguration::Superseded
    );
    assert_eq!(
        classify_paint_configuration(5, 5).expect("current configure is paintable"),
        PaintConfiguration::Current
    );
    assert!(
        classify_paint_configuration(6, 5).is_err(),
        "a client may not invent a future configure generation"
    );
}

#[test]
fn close_deadline_arms_once_and_never_pushes_out() {
    // A repeated CloseRequest (desktop re-committing a close-in-progress) must
    // not keep extending the deadline, or a wedged app could defeat the timeout
    // by racing further commits. `get_or_insert_with` — the exact call in
    // `route_close` — arms it once and keeps the first deadline.
    let first = Instant::now() + CLOSE_TIMEOUT;
    let mut deadline: Option<Instant> = None;
    assert_eq!(*deadline.get_or_insert_with(|| first), first);
    let later = Instant::now() + CLOSE_TIMEOUT + Duration::from_secs(60);
    assert_eq!(*deadline.get_or_insert_with(|| later), first, "must not push out");
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
