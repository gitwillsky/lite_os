use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};

use crate::protocol::{
    self, AnyEvent, AnyRequest, Interface, wl_display, wl_output, wl_registry, wl_shm, wl_surface,
    xdg_toplevel,
};
use crate::wire::{
    DecodeError, DynamicNewId, EncodeError, Fixed, Frame, FrameError, Framer, MAX_MESSAGE_SIZE,
    ObjectId, Reader, Writer,
};

fn id(value: u32) -> ObjectId {
    ObjectId::new(value).unwrap()
}

fn no_fds() -> VecDeque<OwnedFd> {
    VecDeque::new()
}

fn body(bytes: &[u8]) -> &[u8] {
    &bytes[8..]
}

fn pipe_fd() -> OwnedFd {
    std::fs::File::open("/dev/null").unwrap().into()
}

#[test]
fn get_registry_matches_the_wire_example() {
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    assert_eq!(writer.bytes(), [1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0]);
}

#[test]
fn registry_global_event_pads_strings() {
    let mut writer = Writer::new();
    wl_registry::Event::Global {
        name: 1,
        interface: "wl_shm".into(),
        version: 2,
    }
    .encode(id(2), &mut writer)
    .unwrap();
    let bytes = writer.bytes();
    assert_eq!(&bytes[..8], [2, 0, 0, 0, 0, 0, 28, 0]);
    assert_eq!(&bytes[8..12], 1u32.to_le_bytes());
    assert_eq!(&bytes[12..16], 7u32.to_le_bytes());
    assert_eq!(&bytes[16..24], b"wl_shm\0\0");
    assert_eq!(&bytes[24..28], 2u32.to_le_bytes());

    let mut fds = no_fds();
    let event = wl_registry::Event::decode(0, &mut Reader::new(body(bytes), &mut fds)).unwrap();
    assert!(
        matches!(event, wl_registry::Event::Global { name: 1, version: 2, ref interface } if interface == "wl_shm")
    );
}

#[test]
fn untyped_new_id_round_trips_as_three_values() {
    let bind = wl_registry::Request::Bind {
        name: 7,
        id: DynamicNewId {
            interface: "wl_compositor".into(),
            version: 6,
            id: id(3),
        },
    };
    let mut writer = Writer::new();
    bind.encode(id(2), &mut writer).unwrap();
    let mut fds = no_fds();
    let decoded =
        wl_registry::Request::decode(0, &mut Reader::new(body(writer.bytes()), &mut fds)).unwrap();
    let wl_registry::Request::Bind { name, id: new } = decoded;
    assert_eq!(
        (name, new.interface.as_str(), new.version, new.id),
        (7, "wl_compositor", 6, id(3))
    );
}

#[test]
fn nullable_object_and_signed_arguments_round_trip() {
    for buffer in [None, Some(id(9))] {
        let mut writer = Writer::new();
        wl_surface::Request::Attach {
            buffer,
            x: -5,
            y: 6,
        }
        .encode(id(4), &mut writer)
        .unwrap();
        let mut fds = no_fds();
        let decoded =
            wl_surface::Request::decode(1, &mut Reader::new(body(writer.bytes()), &mut fds))
                .unwrap();
        assert!(
            matches!(decoded, wl_surface::Request::Attach { buffer: b, x: -5, y: 6 } if b == buffer)
        );
    }
}

#[test]
fn file_descriptors_travel_out_of_band() {
    let descriptor = pipe_fd();
    let raw = descriptor.as_raw_fd();
    let mut writer = Writer::new();
    wl_shm::Request::CreatePool {
        id: id(5),
        fd: descriptor,
        size: 4096,
    }
    .encode(id(3), &mut writer)
    .unwrap();
    assert_eq!(writer.fds(), [raw]);
    // fd 不占用消息体：头 + new_id + size。
    assert_eq!(writer.bytes().len(), 8 + 4 + 4);

    let mut queue = VecDeque::from([pipe_fd()]);
    let decoded =
        wl_shm::Request::decode(0, &mut Reader::new(body(writer.bytes()), &mut queue)).unwrap();
    assert!(matches!(
        decoded,
        wl_shm::Request::CreatePool { size: 4096, .. }
    ));
    assert!(queue.is_empty());

    let error = wl_shm::Request::decode(0, &mut Reader::new(body(writer.bytes()), &mut no_fds()))
        .unwrap_err();
    assert_eq!(error, DecodeError::MissingFd);
}

#[test]
fn arrays_enums_and_bitfields_use_generated_types() {
    let mut writer = Writer::new();
    xdg_toplevel::Event::Configure {
        width: 640,
        height: 480,
        states: vec![1, 0, 0, 0, 4, 0, 0, 0, 9],
    }
    .encode(id(6), &mut writer)
    .unwrap();
    // 9 字节数组补到 12。
    assert_eq!(writer.bytes().len(), 8 + 4 + 4 + 4 + 12);
    let mut fds = no_fds();
    let decoded =
        xdg_toplevel::Event::decode(0, &mut Reader::new(body(writer.bytes()), &mut fds)).unwrap();
    assert!(
        matches!(decoded, xdg_toplevel::Event::Configure { width: 640, ref states, .. } if states.len() == 9)
    );

    assert_eq!(
        wl_output::Transform::from_u32(3),
        Some(wl_output::Transform::_270)
    );
    assert_eq!(wl_output::Transform::from_u32(8), None);
    assert_eq!(protocol::wl_seat::capability::POINTER, 1);
    assert_eq!(protocol::wl_seat::capability::KEYBOARD, 2);
}

#[test]
fn invalid_enum_values_are_protocol_errors() {
    let mut writer = Writer::new();
    writer.begin(id(8), 0);
    writer.uint(0x1234_5678);
    writer.end().unwrap();
    let mut fds = no_fds();
    let error =
        wl_shm::Event::decode(0, &mut Reader::new(body(writer.bytes()), &mut fds)).unwrap_err();
    assert_eq!(error, DecodeError::InvalidEnum);
}

#[test]
fn malformed_bodies_are_rejected_not_misparsed() {
    let decode = |bytes: &[u8]| {
        wl_registry::Event::decode(0, &mut Reader::new(bytes, &mut no_fds())).unwrap_err()
    };
    let word = |value: u32| value.to_le_bytes();
    // 缺少 version。
    let mut short = Vec::from(word(1));
    short.extend(word(2));
    short.extend(b"a\0\0\0");
    assert_eq!(decode(&short), DecodeError::Truncated);
    // 字符串缺少结尾 NUL。
    let mut no_nul = Vec::from(word(1));
    no_nul.extend(word(2));
    no_nul.extend(b"ab\0x");
    no_nul.extend(word(1));
    assert_eq!(decode(&no_nul), DecodeError::InvalidString);
    // 非 UTF-8。
    let mut bad_utf8 = Vec::from(word(1));
    bad_utf8.extend(word(2));
    bad_utf8.extend([0xFF, 0, 0, 0]);
    bad_utf8.extend(word(1));
    assert_eq!(decode(&bad_utf8), DecodeError::InvalidString);
    // interface 不允许为空。
    let mut null_string = Vec::from(word(1));
    null_string.extend(word(0));
    null_string.extend(word(1));
    assert_eq!(decode(&null_string), DecodeError::UnexpectedNull);
    // 多余字节。
    let mut trailing = Vec::from(word(1));
    trailing.extend(word(2));
    trailing.extend(b"a\0\0\0");
    trailing.extend(word(1));
    trailing.extend(word(0));
    assert_eq!(decode(&trailing), DecodeError::TrailingBytes);
    // 字符串长度声明超过消息体。
    let mut huge = Vec::from(word(1));
    huge.extend(word(u32::MAX));
    assert_eq!(decode(&huge), DecodeError::Truncated);
    // new_id 为 0。
    let error =
        wl_display::Request::decode(1, &mut Reader::new(&word(0), &mut no_fds())).unwrap_err();
    assert_eq!(error, DecodeError::UnexpectedNull);
    assert_eq!(
        wl_display::Request::decode(9, &mut Reader::new(&[], &mut no_fds())).unwrap_err(),
        DecodeError::UnknownOpcode(9)
    );
}

#[test]
fn oversize_messages_roll_back_without_corrupting_the_buffer() {
    let mut writer = Writer::new();
    wl_display::Request::Sync { callback: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    let before = writer.bytes().to_vec();
    let descriptor = pipe_fd();
    let huge = "x".repeat(MAX_MESSAGE_SIZE);
    writer.begin(id(3), 0);
    writer.fd(descriptor.as_raw_fd());
    writer.string(Some(&huge));
    assert_eq!(writer.end(), Err(EncodeError::TooLarge));
    assert_eq!(writer.bytes(), before);
    assert!(writer.fds().is_empty());
    // 回滚后仍可继续写下一条。
    wl_display::Request::Sync { callback: id(4) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    assert_eq!(writer.bytes().len(), before.len() * 2);
}

#[test]
fn writer_consume_drops_sent_prefix() {
    let mut writer = Writer::new();
    let (first, second) = (pipe_fd(), pipe_fd());
    writer.begin(id(1), 0);
    writer.fd(first.as_raw_fd());
    writer.end().unwrap();
    let first_len = writer.bytes().len();
    writer.begin(id(1), 1);
    writer.fd(second.as_raw_fd());
    writer.end().unwrap();
    writer.consume(first_len, 1);
    assert_eq!(writer.fds(), [second.as_raw_fd()]);
    assert_eq!(writer.bytes().len(), first_len);
    assert_eq!(&writer.bytes()[4..6], [1, 0]);
}

#[test]
fn framer_is_independent_of_how_the_stream_is_split() {
    let mut writer = Writer::new();
    wl_display::Request::GetRegistry { registry: id(2) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();
    wl_registry::Event::Global {
        name: 1,
        interface: "wl_shm".into(),
        version: 1,
    }
    .encode(id(2), &mut writer)
    .unwrap();
    wl_display::Request::Sync { callback: id(3) }
        .encode(ObjectId::DISPLAY, &mut writer)
        .unwrap();

    let collect = |chunk: usize| {
        let mut framer = Framer::new();
        let mut frames = Vec::new();
        for piece in writer.bytes().chunks(chunk) {
            framer.push(piece);
            while let Some(frame) = framer.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        frames
    };
    let whole = collect(writer.bytes().len());
    assert_eq!(whole.len(), 3);
    assert_eq!(
        whole[1],
        Frame {
            object: id(2),
            opcode: 0,
            body: writer.bytes()[20..40].to_vec()
        }
    );
    for chunk in [1, 3, 7, 8, 11] {
        assert_eq!(collect(chunk), whole, "chunk size {chunk}");
    }
}

#[test]
fn framer_rejects_unsynchronisable_headers() {
    let header = |object: u32, size: u32, opcode: u32| {
        let mut bytes = Vec::from(object.to_le_bytes());
        bytes.extend(((size << 16) | opcode).to_le_bytes());
        bytes
    };
    for (bytes, expected) in [
        (header(0, 8, 0), FrameError::ZeroObject),
        (header(1, 4, 0), FrameError::InvalidSize(4)),
        (header(1, 10, 0), FrameError::InvalidSize(10)),
        (header(1, 65535, 0), FrameError::InvalidSize(65535)),
    ] {
        let mut framer = Framer::new();
        framer.push(&bytes);
        assert_eq!(framer.next_frame().unwrap_err(), expected);
    }
    let mut waiting = Framer::new();
    waiting.push(&header(1, 12, 0));
    assert_eq!(waiting.next_frame().unwrap(), None);
    // Wire size is 16-bit; a valid message may cross 4 KiB and arrive in fragments.
    let mut large = Framer::new();
    large.push(&header(1, 4100, 0));
    assert_eq!(large.next_frame().unwrap(), None);
    large.push(&vec![0; 4092]);
    assert_eq!(large.next_frame().unwrap().unwrap().body.len(), 4092);
}

#[test]
fn interface_table_covers_the_vendored_protocols() {
    for interface in Interface::ALL {
        assert_eq!(Interface::from_name(interface.name()), Some(*interface));
        assert!(interface.version() >= 1);
    }
    assert_eq!(Interface::from_name("wl_compositor").unwrap().version(), 6);
    assert_eq!(Interface::from_name("xdg_wm_base").unwrap().version(), 7);
    assert!(Interface::from_name("wp_viewporter").is_some());
    assert_eq!(Interface::from_name("nonexistent"), None);

    let mut fds = no_fds();
    let request = protocol::decode_request(
        Interface::WlDisplay,
        1,
        &mut Reader::new(&2u32.to_le_bytes(), &mut fds),
    )
    .unwrap();
    assert!(matches!(
        request,
        AnyRequest::WlDisplay(wl_display::Request::GetRegistry { .. })
    ));
    let event = protocol::decode_event(
        Interface::WlCallback,
        0,
        &mut Reader::new(&5u32.to_le_bytes(), &mut fds),
    )
    .unwrap();
    assert!(matches!(event, AnyEvent::WlCallback(_)));
}

#[test]
fn destructors_and_versions_are_exposed() {
    assert!(wl_surface::Request::Destroy.is_destructor());
    assert!(!wl_surface::Request::Commit.is_destructor());
    assert_eq!(wl_surface::Request::Commit.opcode(), 6);
    assert_eq!(wl_surface::Request::Offset { x: 0, y: 0 }.since(), 5);
    assert_eq!(wl_surface::Event::Enter { output: id(1) }.since(), 1);
}

#[test]
fn fixed_point_conversions() {
    assert_eq!(Fixed::from_int(3).0, 768);
    assert_eq!(Fixed::from_f64(1.5).0, 384);
    assert_eq!(Fixed::from_f64(-0.00390625).0, -1);
    assert_eq!(Fixed(-384).to_f64(), -1.5);
    assert_eq!(Fixed(-384).to_int(), -1);
}

#[test]
fn arbitrary_bodies_never_panic_any_decoder() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for interface in Interface::ALL {
        for opcode in 0..24 {
            for length in [0usize, 4, 8, 16, 33, 64] {
                let bytes: Vec<u8> = (0..length).map(|_| next() as u8).collect();
                let mut fds = no_fds();
                let _ = protocol::decode_request(
                    *interface,
                    opcode,
                    &mut Reader::new(&bytes, &mut fds),
                );
                let _ =
                    protocol::decode_event(*interface, opcode, &mut Reader::new(&bytes, &mut fds));
            }
        }
    }
}
