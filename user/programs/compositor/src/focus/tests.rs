use wayland_wire::wire::{Fixed, ObjectId};

use super::{KeyTracker, local_position, window_configure};
use crate::desktop::{Initial, Stack};
use crate::wayland::ToplevelState;
use crate::{desktop::Mode, wayland::ClientId};

#[test]
fn local_position_is_in_client_logical_pixels() {
    let (x, y) = local_position((40, 40), (50, 60), 1);
    assert_eq!((x, y), (Fixed::from_int(10), Fixed::from_int(20)));
    // 2x：每个客户端逻辑像素占 2 个物理像素，奇数偏移得到半像素。
    let (x, y) = local_position((80, 80), (91, 80), 2);
    assert_eq!((x, y), (Fixed::from_f64(5.5), Fixed::from_int(0)));
    // 窗口左上之外的负偏移也能表示（指针捕获时有用）。
    assert_eq!(
        local_position((40, 40), (30, 40), 1).0,
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

const SCREEN: (i32, i32) = (1280, 800);
const FULL: (i32, i32, i32, i32) = (0, 0, 1280, 800);

fn stack_with_two() -> Stack {
    let mut stack = Stack::default();
    let surface = ObjectId::new(5).unwrap();
    stack.sync(
        &[
            (ClientId(1), surface, 200, 100, Initial::Cascade),
            (ClientId(2), surface, 200, 100, Initial::Cascade),
        ],
        SCREEN,
        1,
    );
    stack
}

#[test]
fn a_normal_window_is_told_its_activation_only() {
    let stack = stack_with_two();
    let surface = ObjectId::new(5).unwrap();
    let active = window_configure(&stack, (ClientId(2), surface), 1, None, false);
    assert_eq!(
        active,
        (
            0,
            0,
            ToplevelState {
                activated: true,
                ..ToplevelState::default()
            }
        )
    );
    let inactive = window_configure(&stack, (ClientId(1), surface), 1, None, false);
    assert_eq!(inactive, (0, 0, ToplevelState::default()));
}

#[test]
fn maximized_and_fullscreen_windows_keep_their_real_size_when_reconfigured() {
    let mut stack = stack_with_two();
    let surface = ObjectId::new(5).unwrap();
    stack.set_mode(ClientId(1), surface, Mode::Maximized, FULL, FULL);
    // 失去激活的最大化窗口仍然是最大化，并带着屏幕尺寸（逻辑像素，2x 下减半）。
    assert_eq!(
        window_configure(&stack, (ClientId(1), surface), 1, None, false),
        (
            1280,
            800,
            ToplevelState {
                maximized: true,
                ..ToplevelState::default()
            }
        )
    );
    assert_eq!(
        window_configure(&stack, (ClientId(1), surface), 2, None, false).0,
        640
    );
    stack.set_mode(ClientId(1), surface, Mode::Fullscreen, FULL, FULL);
    assert!(
        window_configure(&stack, (ClientId(1), surface), 1, None, false)
            .2
            .fullscreen
    );
}

#[test]
fn resizing_carries_the_target_size_and_the_resizing_flag() {
    let stack = stack_with_two();
    let surface = ObjectId::new(5).unwrap();
    let (width, height, state) =
        window_configure(&stack, (ClientId(2), surface), 2, Some((500, 301)), true);
    assert_eq!((width, height), (250, 150));
    assert!(state.resizing && state.activated && !state.maximized);
}

#[test]
fn a_vanished_window_gets_a_neutral_configure() {
    let stack = stack_with_two();
    let gone = ObjectId::new(9).unwrap();
    assert_eq!(
        window_configure(&stack, (ClientId(7), gone), 1, None, false),
        (0, 0, ToplevelState::default())
    );
}

#[test]
fn restoring_from_maximized_carries_the_size_without_the_resizing_flag() {
    let stack = stack_with_two();
    let surface = ObjectId::new(5).unwrap();
    let (width, height, state) =
        window_configure(&stack, (ClientId(2), surface), 1, Some((300, 200)), false);
    assert_eq!((width, height), (300, 200));
    assert!(!state.resizing && state.activated);
}

#[test]
fn secure_attention_requires_hardware_control_alt_delete_and_desktop_shortcuts_are_separate() {
    let mut seat = super::Seat::default();
    assert!(!seat.secure_attention(111, true));
    seat.keys.track(29, true);
    assert!(!seat.secure_attention(111, true));
    assert!(seat.desktop_shortcut(1, true));
    seat.keys.track(56, true);
    assert!(seat.secure_attention(111, true));
    assert!(!seat.secure_attention(111, false));
    assert!(!seat.secure_attention(14, true));
    assert!(!seat.desktop_shortcut(1, true));
    seat.keys.track(29, false);
    assert!(!seat.secure_attention(111, true));
}
