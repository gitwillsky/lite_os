use super::{input::PointerCapture, renderer::HitRegion};

fn hit(node_id: u64, pointer_move: u64, pointer_up: u64) -> HitRegion {
    HitRegion {
        node_id,
        x: 0.0,
        y: 0.0,
        width: 10.0,
        height: 10.0,
        pointer_down: Some(1),
        pointer_move: Some(pointer_move),
        pointer_up: Some(pointer_up),
        click: None,
        double_click: None,
        pointer_enter: None,
        pointer_leave: None,
        context_menu: None,
        wheel: None,
        key_down: None,
        cursor: 0,
        editable: None,
    }
}

#[test]
fn pointer_capture_resolves_rebuilt_node_listeners() {
    let capture = PointerCapture { node_id: 7 };
    let initial = [hit(7, 11, 12)];
    let rebuilt = [hit(7, 21, 22)];

    assert_eq!(capture.move_listener(&initial), Some(11));
    assert_eq!(capture.move_listener(&rebuilt), Some(21));
    assert_eq!(capture.up_listener(&rebuilt), Some(22));
}

#[test]
fn pointer_capture_releases_when_the_node_unmounts() {
    let capture = PointerCapture { node_id: 7 };
    let rebuilt = [hit(8, 31, 32)];

    assert_eq!(capture.move_listener(&rebuilt), None);
    assert_eq!(capture.up_listener(&rebuilt), None);
}
