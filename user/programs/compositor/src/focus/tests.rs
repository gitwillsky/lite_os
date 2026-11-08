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
            (ClientId(1), surface, 1, 200, 100, Initial::Cascade),
            (ClientId(2), surface, 1, 200, 100, Initial::Cascade),
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
    let mut hardware = keyboard::Modifiers::default();
    assert!(!super::Seat::secure_attention(hardware, 111, true));
    seat.keys.track(29, true);
    assert!(seat.desktop_shortcut(1, true));
    seat.keys.track(56, true);
    assert!(!seat.desktop_shortcut(1, true));
    assert!(!super::Seat::secure_attention(hardware, 111, true));
    hardware.apply(29, 1);
    assert!(!super::Seat::secure_attention(hardware, 111, true));
    hardware.apply(56, 1);
    assert!(super::Seat::secure_attention(hardware, 111, true));
    assert!(!super::Seat::secure_attention(hardware, 111, false));
    assert!(!super::Seat::secure_attention(hardware, 14, true));
    hardware.apply(29, 0);
    assert!(!super::Seat::secure_attention(hardware, 111, true));
}

// 单个 window_configure 用例无法发现外壳借走键盘后的漏发；这里观察真实 Client 编码输出，
// 不把最后激活窗口和当前 wl_keyboard 目标当成同一个状态。
#[test]
fn shell_keyboard_borrow_does_not_lose_the_previous_window_activation() {
    use crate::wayland::{Output, Server};
    use linux_uapi::unix::PollEvents;
    use std::{
        collections::VecDeque,
        io::{self, Read},
        os::unix::net::UnixStream,
    };
    use wayland_wire::{
        protocol::{
            self, AnyEvent, Interface, wl_compositor, wl_display, wl_registry, xdg_surface,
            xdg_toplevel, xdg_wm_base,
        },
        wire::{DynamicNewId, Framer, Reader, Writer},
    };
    let id = |value| ObjectId::new(value).unwrap();
    let path = std::env::temp_dir().join(format!("wl-activation-{}", std::process::id()));
    let mut server = Server::bind(
        &path,
        Output {
            width: SCREEN.0,
            height: SCREEN.1,
            scale: 1,
        },
    )
    .unwrap();
    let mut peer = UnixStream::connect(&path).unwrap();
    peer.set_nonblocking(true).unwrap();
    std::fs::remove_file(&path).unwrap();
    server.dispatch(&[PollEvents::READ]).unwrap();
    let mut wire = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut wire)
        .unwrap();
    for (name, interface, object) in [(1, "wl_compositor", 3), (3, "xdg_wm_base", 20)] {
        wl_registry::Request::Bind {
            name,
            id: DynamicNewId {
                interface: interface.into(),
                version: 3,
                id: id(object),
            },
        }
        .encode(id(2), &mut wire)
        .unwrap();
    }
    for (surface, xdg, top) in [(5, 21, 22), (6, 23, 24)] {
        wl_compositor::Request::CreateSurface { id: id(surface) }
            .encode(id(3), &mut wire)
            .unwrap();
        xdg_wm_base::Request::GetXdgSurface {
            id: id(xdg),
            surface: id(surface),
        }
        .encode(id(20), &mut wire)
        .unwrap();
        xdg_surface::Request::GetToplevel { id: id(top) }
            .encode(id(xdg), &mut wire)
            .unwrap();
    }
    wl_compositor::Request::CreateSurface { id: id(7) }
        .encode(id(3), &mut wire)
        .unwrap();
    server.with_client(ClientId(1), |client| {
        client.receive(wire.bytes(), Vec::new()).unwrap();
        let length = client.output().0.len();
        client.consume_output(length);
    });
    let mut frames = Framer::new();
    let mut configs = || {
        let mut found = Vec::new();
        let mut bytes = [0; 4096];
        loop {
            match peer.read(&mut bytes) {
                Ok(0) => panic!("peer closed"),
                Ok(count) => frames.push(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        while let Some(frame) = frames.next_frame().unwrap() {
            if [22, 24].contains(&frame.object.get()) && frame.opcode == 0 {
                let mut fds = VecDeque::new();
                let event = protocol::decode_event(
                    Interface::XdgToplevel,
                    frame.opcode,
                    &mut Reader::new(&frame.body, &mut fds),
                )
                .unwrap();
                if let AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { states, .. }) = event
                {
                    found.push((
                        frame.object,
                        states
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|raw| *raw == 4u32.to_ne_bytes()),
                    ));
                }
            }
        }
        found
    };
    let first = (ClientId(1), id(5), 1, 200, 100, Initial::Cascade);
    let second = (ClientId(1), id(6), 1, 200, 100, Initial::Cascade);
    let mut stack = Stack::default();
    stack.sync(&[first], SCREEN, 1);
    let mut seat = super::Seat::default();
    seat.sync_keyboard(&mut server, &stack, 1, None);
    assert_eq!(configs(), [(id(22), true)]);
    let shell = Some((ClientId(1), id(7)));
    seat.sync_keyboard(&mut server, &stack, 1, shell);
    let borrowed = configs();
    stack.sync(&[first, second], SCREEN, 1);
    seat.sync_keyboard(&mut server, &stack, 1, shell);
    assert_eq!(
        configs(),
        [(id(22), false), (id(24), true)],
        "activation changes even while the keyboard target stays on the shell"
    );
    assert!(
        borrowed.is_empty(),
        "keyboard borrow must preserve activation without republishing it"
    );
    seat.sync_keyboard(&mut server, &stack, 1, None);
    assert!(
        configs().is_empty(),
        "returning the keyboard must not republish activation"
    );
    let replacement = (second.0, second.1, 2, second.3, second.4, second.5);
    stack.sync(&[first, replacement], SCREEN, 1);
    assert!(seat.sync_keyboard(&mut server, &stack, 1, None));
    assert_eq!(
        configs(),
        [(id(24), true)],
        "new mapping must republish activation even with the same raw target"
    );
}
