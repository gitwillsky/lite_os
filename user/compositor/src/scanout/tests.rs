use display_proto::{Rect, SceneNodeKind};

use crate::session::Node;

use super::source_buffer_id;

fn node(kind: SceneNodeKind, window_group: u32, buffer_id: u32) -> Node {
    Node {
        kind,
        window_group,
        buffer_id,
        bounds: Rect::default(),
        clip: Rect::default(),
        opaque: None,
        damage: Vec::new(),
        corner_radius: 0,
    }
}

#[test]
fn move_uses_underlay_only_for_desktop_pixels_outside_the_moving_group() {
    let active = Some((9, (40, 20), 77));
    assert_eq!(
        source_buffer_id(&node(SceneNodeKind::Pixels, 0, 11), active),
        77
    );
    assert_eq!(
        source_buffer_id(&node(SceneNodeKind::Pixels, 8, 11), active),
        77
    );
    assert_eq!(
        source_buffer_id(&node(SceneNodeKind::Pixels, 9, 11), active),
        11
    );
    assert_eq!(
        source_buffer_id(&node(SceneNodeKind::ForeignSurface, 9, 22), active),
        22
    );
}
