use wayland_wire::{
    protocol::{AnyEvent, wl_keyboard, wl_pointer, wl_seat},
    wire::{Fixed, Framer},
};

use super::*;

pub(super) const SEAT: u32 = 40;
const POINTER: u32 = 41;
pub(super) const KEYBOARD: u32 = 42;
const OBJECTS: [(u32, Interface); 3] = [
    (SEAT, Interface::WlSeat),
    (POINTER, Interface::WlPointer),
    (KEYBOARD, Interface::WlKeyboard),
];

fn bind_seat(harness: &mut Harness, version: u32) -> Vec<Event> {
    harness
        .send(|w| {
            wl_registry::Request::Bind {
                name: 5,
                id: DynamicNewId {
                    interface: "wl_seat".into(),
                    version,
                    id: id(SEAT),
                },
            }
            .encode(id(2), w)
            .unwrap()
        })
        .unwrap();
    harness.events(&OBJECTS)
}

#[test]
fn binding_announces_pointer_and_keyboard_and_the_name() {
    let mut harness = Harness::bound();
    let events = bind_seat(&mut harness, 7);
    assert!(matches!(
        events[0].event,
        AnyEvent::WlSeat(wl_seat::Event::Capabilities { capabilities: 3 })
    ));
    assert!(
        matches!(&events[1].event, AnyEvent::WlSeat(wl_seat::Event::Name { name }) if name == "seat0")
    );
    let mut harness = Harness::bound();
    assert_eq!(
        bind_seat(&mut harness, 1).len(),
        1,
        "no name before version 2"
    );
}

#[test]
fn touch_is_refused_with_missing_capability() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    assert!(
        harness
            .send(|w| wl_seat::Request::GetTouch { id: id(43) }
                .encode(id(SEAT), w)
                .unwrap())
            .is_err()
    );
    harness.expect_error(wl_seat::Error::MissingCapability as u32);
}

#[test]
fn pointer_events_are_framed_and_go_to_every_pointer_object() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    harness
        .send(|w| {
            wl_seat::Request::GetPointer { id: id(POINTER) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| {
            wl_seat::Request::GetPointer { id: id(44) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    harness.create_surface(5);
    harness
        .client
        .pointer_enter(7, id(5), Fixed::from_int(3), Fixed::from_f64(4.5));
    harness
        .client
        .pointer_motion(100, Fixed::from_int(10), Fixed::from_int(11));
    harness.client.pointer_button(8, 101, 0x110, true);
    harness.client.pointer_leave(9, id(5));
    let mut interfaces = OBJECTS.to_vec();
    interfaces.push((44, Interface::WlPointer));
    let events = harness.events(&interfaces);
    // 两个 pointer 对象 × (4 个事件 + 4 个 frame)。
    assert_eq!(events.len(), 16);
    let for_first: Vec<&wl_pointer::Event> = events
        .iter()
        .filter(|event| event.object == POINTER)
        .map(|event| match &event.event {
            AnyEvent::WlPointer(event) => event,
            _ => panic!("unexpected"),
        })
        .collect();
    assert!(matches!(
        for_first[0],
        wl_pointer::Event::Enter {
            serial: 7,
            surface_x: Fixed(768),
            surface_y: Fixed(1152),
            ..
        }
    ));
    assert!(matches!(for_first[1], wl_pointer::Event::Frame));
    assert!(matches!(
        for_first[2],
        wl_pointer::Event::Motion { time: 100, .. }
    ));
    assert!(matches!(
        for_first[4],
        wl_pointer::Event::Button {
            serial: 8,
            time: 101,
            button: 0x110,
            state: wl_pointer::ButtonState::Pressed
        }
    ));
    assert!(matches!(
        for_first[6],
        wl_pointer::Event::Leave { serial: 9, .. }
    ));
}

#[test]
fn an_old_pointer_binding_gets_no_frame_events() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 4);
    harness
        .send(|w| {
            wl_seat::Request::GetPointer { id: id(POINTER) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    harness.create_surface(5);
    harness
        .client
        .pointer_motion(1, Fixed::from_int(1), Fixed::from_int(1));
    assert_eq!(harness.events(&OBJECTS).len(), 1);
}

#[test]
fn clients_without_input_objects_receive_nothing() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    harness.create_surface(5);
    harness
        .client
        .pointer_enter(1, id(5), Fixed::from_int(0), Fixed::from_int(0));
    harness.client.keyboard_key(1, 1, 30, true);
    harness.client.keyboard_modifiers(2, 1, 0);
    assert!(harness.events(&OBJECTS).is_empty());
}

/// 取走输出段并解码，同时返回随段发送的描述符。
fn drain(harness: &mut Harness) -> (Vec<u8>, Vec<bool>) {
    let mut bytes = Vec::new();
    let mut had_fd = Vec::new();
    loop {
        let (segment, fd) = harness.client.output();
        if segment.is_empty() {
            return (bytes, had_fd);
        }
        had_fd.push(fd.is_some());
        bytes.extend_from_slice(segment);
        let length = segment.len();
        harness.client.consume_output(length);
    }
}

#[test]
fn the_keymap_fd_travels_with_its_own_message_segment() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    harness.create_surface(5);
    harness
        .send(|w| {
            wl_seat::Request::GetKeyboard { id: id(KEYBOARD) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    // 先建立标准 enter/modifiers，再排队普通 key；第二个 keymap 的 fd 不在偏移 0。
    harness.client.keyboard_enter(1, id(5), &[]);
    harness.client.keyboard_modifiers(1, 0, 0);
    harness.client.keyboard_key(1, 1, 30, true);
    harness
        .send(|w| {
            wl_seat::Request::GetKeyboard { id: id(45) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    let (bytes, had_fd) = drain(&mut harness);
    // 段序：[keymap + repeat_info + key（含 fd）]、[第二个 keymap 之前的字节无 fd 不会出现]；
    // 关键不变量：每个带 fd 的段都以对应 keymap 消息开头，并且 fd 数等于 keymap 数。
    assert_eq!(had_fd.iter().filter(|had| **had).count(), 2);
    let mut framer = Framer::new();
    framer.push(&bytes);
    let mut keymaps = 0;
    while let Some(frame) = framer.next_frame().unwrap() {
        if frame.object.get() == KEYBOARD && frame.opcode == 0
            || frame.object.get() == 45 && frame.opcode == 0
        {
            keymaps += 1;
        }
    }
    assert_eq!(keymaps, 2);
}

#[test]
fn keyboard_focus_events_carry_the_pressed_keys_and_modifiers() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    harness
        .send(|w| {
            wl_seat::Request::GetKeyboard { id: id(KEYBOARD) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    drain(&mut harness);
    harness.create_surface(5);
    harness.client.keyboard_enter(3, id(5), &[30, 42]);
    harness.client.keyboard_modifiers(4, 1, 2);
    harness.client.keyboard_key(5, 77, 30, false);
    harness.client.keyboard_leave(6, id(5));
    let events = harness.events(&OBJECTS);
    assert!(
        matches!(&events[0].event, AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { serial: 3, keys, .. }) if keys == &[30, 0, 0, 0, 42, 0, 0, 0])
    );
    assert!(matches!(
        events[1].event,
        AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers {
            serial: 4,
            mods_depressed: 1,
            mods_locked: 2,
            ..
        })
    ));
    assert!(matches!(
        events[2].event,
        AnyEvent::WlKeyboard(wl_keyboard::Event::Key {
            serial: 5,
            time: 77,
            key: 30,
            state: wl_keyboard::KeyState::Released
        })
    ));
    assert!(matches!(
        events[3].event,
        AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { serial: 6, .. })
    ));
}

#[test]
fn a_late_keyboard_gets_its_own_enter_before_keys_without_reentering_existing_objects() {
    let mut harness = Harness::bound();
    bind_seat(&mut harness, 7);
    harness.create_surface(5);
    // 焦点先于键盘绑定：当时没有资源可以接收 enter。
    harness.client.keyboard_enter(1, id(5), &[29]);
    harness
        .send(|w| {
            wl_seat::Request::GetKeyboard { id: id(KEYBOARD) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    drain(&mut harness);
    harness.client.keyboard_key(2, 77, 30, true);
    harness.client.keyboard_modifiers(2, 1, 0);
    assert!(
        harness.events(&OBJECTS).is_empty(),
        "unentered resource cannot receive keys/modifiers"
    );
    harness.client.keyboard_enter(3, id(5), &[29]);
    harness.client.keyboard_modifiers(3, 1, 0);
    let events = harness.events(&OBJECTS);
    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0].event, AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { keys, .. }) if keys == &[29, 0, 0, 0])
    );
    assert!(matches!(
        events[1].event,
        AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers {
            mods_depressed: 1,
            ..
        })
    ));
    harness
        .send(|w| {
            wl_seat::Request::GetKeyboard { id: id(45) }
                .encode(id(SEAT), w)
                .unwrap()
        })
        .unwrap();
    drain(&mut harness);
    harness.client.keyboard_enter(4, id(5), &[29]);
    harness.client.keyboard_modifiers(4, 1, 0);
    let mut objects = OBJECTS.to_vec();
    objects.push((45, Interface::WlKeyboard));
    let events = harness.events(&objects);
    let entered: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { .. })
            )
        })
        .map(|event| event.object)
        .collect();
    assert_eq!(entered, [45]);
    harness.client.keyboard_leave(5, id(5));
    harness.events(&objects);
    harness
        .send(|w| {
            wl_keyboard::Request::Release
                .encode(id(KEYBOARD), w)
                .unwrap()
        })
        .unwrap();
    harness
        .send(|w| wl_keyboard::Request::Release.encode(id(45), w).unwrap())
        .unwrap();
}
