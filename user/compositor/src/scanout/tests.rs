use display_proto::{Rect, SceneNodeKind};

use crate::session::Node;

use super::{moving_group_damage, source_buffer_id};

fn node(kind: SceneNodeKind, window_group: u32, buffer_id: u32) -> Node {
    Node {
        kind,
        window_group,
        buffer_id,
        bounds: Rect::default(),
        clip: Rect::default(),
        clip_masks: Vec::new(),
        opaque: None,
        damage: Vec::new(),
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

fn rect(x: i32, y: i32) -> Rect {
    Rect {
        x,
        y,
        width: 100,
        height: 50,
    }
}

#[test]
fn moving_group_damage_covers_canonical_current_and_stale_positions() {
    // The fast-drag ghost: a buffer last painted the group at (40, 20), the
    // pointer has since moved the offset to (160, 120), and a concurrently
    // submitted scene triggers a full compose. The pre-fix union covered only
    // canonical ∪ current — the stale rect at (40, 20) survived the flip and
    // no later old∪new move damage ever repainted it.
    let damage = moving_group_damage(Some(rect(0, 0)), (160, 120), Some(rect(40, 20)))
        .expect("move damage exists");

    assert!(contains(damage, rect(0, 0)), "canonical position repaints");
    assert!(contains(damage, rect(160, 120)), "current offset repaints");
    assert!(
        contains(damage, rect(40, 20)),
        "stale temporary position repaints (the ghost)"
    );
}

#[test]
fn moving_group_damage_without_active_move_still_cleans_stale_paint() {
    // Grab finished: the canonical scene compose carries no temporary
    // transform, but the stale temp rect must still be cleaned once.
    let damage = moving_group_damage(None, (0, 0), Some(rect(40, 20))).expect("stale cleans");
    assert!(contains(damage, rect(40, 20)));
    assert_eq!(moving_group_damage(None, (0, 0), None), None);
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width as i32 <= outer.x + outer.width as i32
        && inner.y + inner.height as i32 <= outer.y + outer.height as i32
}
