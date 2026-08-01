use super::{
    input::{PointerCapture, bubbling_listener_ids},
    renderer::HitRegion,
};

fn hit(node_id: u64, pointer_move: u64, pointer_up: u64) -> HitRegion {
    HitRegion {
        node_id,
        parent_node_id: None,
        window_group: None,
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
        range: None,
        button: false,
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

#[test]
fn event_route_bubbles_only_through_dom_ancestors() {
    let mut root = hit(1, 0, 0);
    root.click = Some(10);
    root.key_down = Some(11);
    let mut child = hit(2, 0, 0);
    child.parent_node_id = Some(1);
    child.click = Some(20);
    let mut overlapping_sibling = hit(3, 0, 0);
    overlapping_sibling.click = Some(30);
    let hits = [root, overlapping_sibling, child];

    assert_eq!(
        bubbling_listener_ids(&hits, Some(2), |hit| hit.click),
        vec![20, 10]
    );
    assert_eq!(
        bubbling_listener_ids(&hits, Some(2), |hit| hit.key_down),
        vec![11]
    );
}
