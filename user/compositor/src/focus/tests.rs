use wayland_wire::wire::{Fixed, ObjectId};

use super::{KeyTracker, local_position};
use crate::{desktop::Placement, wayland::ClientId};

fn placement(x: i32, y: i32) -> Placement {
    Placement {
        client: ClientId(1),
        surface: ObjectId::new(5).unwrap(),
        x,
        y,
        width: 200,
        height: 100,
    }
}

#[test]
fn local_position_is_in_client_logical_pixels() {
    let (x, y) = local_position(&placement(40, 40), (50, 60), 1);
    assert_eq!((x, y), (Fixed::from_int(10), Fixed::from_int(20)));
    // 2x：每个客户端逻辑像素占 2 个物理像素，奇数偏移得到半像素。
    let (x, y) = local_position(&placement(80, 80), (91, 80), 2);
    assert_eq!((x, y), (Fixed::from_f64(5.5), Fixed::from_int(0)));
    // 窗口左上之外的负偏移也能表示（指针捕获时有用）。
    assert_eq!(
        local_position(&placement(40, 40), (30, 40), 1).0,
        Fixed::from_int(-10)
    );
}

#[test]
fn modifiers_are_tracked_and_changes_are_reported() {
    let mut keys = KeyTracker::default();
    assert!(keys.track(56, true), "left alt down");
    assert!(keys.modifiers().alt);
    assert!(
        !keys.track(30, true),
        "a plain key does not change modifiers"
    );
    assert!(keys.track(56, false));
    assert!(!keys.modifiers().alt);
    assert!(keys.track(58, true), "caps lock toggles on");
    assert!(keys.modifiers().caps);
    assert!(!keys.track(58, false), "its release is not a change");
    assert!(keys.track(58, true));
    assert!(!keys.modifiers().caps);
}

#[test]
fn pressed_keys_are_a_set_without_duplicates() {
    let mut keys = KeyTracker::default();
    keys.track(30, true);
    keys.track(30, true);
    keys.track(31, true);
    assert_eq!(keys.pressed(), [30, 31]);
    keys.track(30, false);
    assert_eq!(keys.pressed(), [31]);
    keys.track(99, false);
    assert_eq!(keys.pressed(), [31]);
}

#[test]
fn consumed_keys_swallow_exactly_one_release() {
    let mut keys = KeyTracker::default();
    assert!(!keys.take_consumed(62));
    keys.consume(62);
    keys.consume(62);
    assert!(keys.take_consumed(62));
    assert!(
        !keys.take_consumed(62),
        "the mark is cleared after the release"
    );
}
