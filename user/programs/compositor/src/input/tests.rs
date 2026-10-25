use linux_uapi::input::AbsoluteRange;

use super::{Decoder, Event, Pointer};

const SYN: (u16, u16, i32) = (0, 0, 0);
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const KEY_A: u16 = 30;

fn tablet() -> Decoder {
    let range = AbsoluteRange {
        minimum: 0,
        maximum: 32767,
    };
    Decoder::new(Some([range, range]))
}

fn run(decoder: &mut Decoder, pointer: &mut Pointer, events: &[(u16, u16, i32)]) -> Vec<Event> {
    let mut out = Vec::new();
    for &(kind, code, value) in events {
        decoder.feed(kind, code, value, pointer, &mut out);
    }
    out
}

#[test]
fn motion_batch_keeps_the_latest_position_without_crossing_input_barriers() {
    let motion = |x| Event::PointerMoved { x, y: x };
    let press = Event::Button {
        code: BTN_LEFT,
        pressed: true,
    };
    let key = Event::Key {
        code: KEY_A,
        pressed: false,
    };
    let wheel = Event::Wheel {
        vertical: 1,
        horizontal: 0,
    };
    let mut events = vec![
        motion(1),
        motion(2),
        press,
        motion(3),
        motion(4),
        key,
        motion(5),
        wheel,
        motion(6),
        motion(7),
    ];
    super::coalesce_motion(&mut events);
    assert_eq!(
        events,
        [
            motion(2),
            press,
            motion(4),
            key,
            motion(5),
            wheel,
            motion(7)
        ]
    );
}

#[test]
fn tablet_maps_the_full_range_onto_the_screen_edges() {
    let mut pointer = Pointer::new(1280, 800);
    let mut decoder = tablet();
    let events = run(
        &mut decoder,
        &mut pointer,
        &[(3, ABS_X, 0), (3, ABS_Y, 0), SYN],
    );
    assert_eq!(events, [Event::PointerMoved { x: 0, y: 0 }]);
    let events = run(
        &mut decoder,
        &mut pointer,
        &[(3, ABS_X, 32767), (3, ABS_Y, 32767), SYN],
    );
    assert_eq!(events, [Event::PointerMoved { x: 1279, y: 799 }]);
    // 中点落在屏幕中间附近（整数除法允许 1 像素误差）。
    run(
        &mut decoder,
        &mut pointer,
        &[(3, ABS_X, 16384), (3, ABS_Y, 16384), SYN],
    );
    let (x, y) = pointer.position();
    assert!(
        (639..=640).contains(&x) && (399..=400).contains(&y),
        "{x},{y}"
    );
}

#[test]
fn motion_is_committed_only_at_sync_and_only_when_it_changes() {
    let mut pointer = Pointer::new(100, 100);
    let mut decoder = tablet();
    assert!(run(&mut decoder, &mut pointer, &[(3, ABS_X, 100)]).is_empty());
    assert_eq!(run(&mut decoder, &mut pointer, &[SYN]).len(), 1);
    assert!(
        run(&mut decoder, &mut pointer, &[SYN]).is_empty(),
        "no change, no event"
    );
    // 只更新一个轴时另一个轴保持。
    let before = pointer.position();
    run(&mut decoder, &mut pointer, &[(3, ABS_Y, 0), SYN]);
    assert_eq!(pointer.position().0, before.0);
}

#[test]
fn out_of_range_absolute_values_are_clamped() {
    let mut pointer = Pointer::new(100, 100);
    let mut decoder = tablet();
    run(
        &mut decoder,
        &mut pointer,
        &[(3, ABS_X, -500), (3, ABS_Y, 999_999), SYN],
    );
    assert_eq!(pointer.position(), (0, 99));
}

#[test]
fn relative_motion_accumulates_per_report_and_clamps_to_the_screen() {
    let mut pointer = Pointer::new(100, 100);
    let mut mouse = Decoder::new(None);
    let events = run(
        &mut mouse,
        &mut pointer,
        &[(2, 0, 5), (2, 0, 7), (2, 1, -3), SYN],
    );
    assert_eq!(events, [Event::PointerMoved { x: 62, y: 47 }]);
    run(
        &mut mouse,
        &mut pointer,
        &[(2, 0, 1000), (2, 1, -1000), SYN],
    );
    assert_eq!(pointer.position(), (99, 0));
}

#[test]
fn a_relative_device_ignores_absolute_axes() {
    let mut pointer = Pointer::new(100, 100);
    let mut mouse = Decoder::new(None);
    assert!(run(&mut mouse, &mut pointer, &[(3, ABS_X, 5), SYN]).is_empty());
    assert_eq!(pointer.position(), (50, 50));
}

#[test]
fn buttons_and_keys_are_distinguished_and_autorepeat_is_dropped() {
    let mut pointer = Pointer::new(100, 100);
    let mut decoder = Decoder::new(None);
    let events = run(
        &mut decoder,
        &mut pointer,
        &[
            (1, BTN_LEFT, 1),
            (1, BTN_LEFT, 0),
            (1, KEY_A, 1),
            (1, KEY_A, 2),
            (1, KEY_A, 0),
            (1, 0x118, 1),
        ],
    );
    assert_eq!(
        events,
        [
            Event::Button {
                code: BTN_LEFT,
                pressed: true
            },
            Event::Button {
                code: BTN_LEFT,
                pressed: false
            },
            Event::Key {
                code: KEY_A,
                pressed: true
            },
            Event::Key {
                code: KEY_A,
                pressed: false
            },
            Event::Key {
                code: 0x118,
                pressed: true
            },
        ]
    );
}

#[test]
fn degenerate_absolute_range_maps_to_zero_instead_of_dividing_by_zero() {
    let flat = AbsoluteRange {
        minimum: 5,
        maximum: 5,
    };
    let mut pointer = Pointer::new(100, 100);
    let mut decoder = Decoder::new(Some([flat, flat]));
    run(&mut decoder, &mut pointer, &[(3, ABS_X, 5), SYN]);
    assert_eq!(pointer.position().0, 0);
}
