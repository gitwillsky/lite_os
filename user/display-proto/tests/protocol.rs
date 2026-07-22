use std::{io::Write, os::unix::net::UnixStream};

use display_proto::{
    Accepted, AppOpened, BufferAlloc, HelloApp, InputKey, InputPointer, MessageKind,
    PROTOCOL_VERSION, PointerPhase, Presented, Rect, Rectangles, SceneCommit, SceneNode,
    SceneNodeKind, Size, SurfaceCommit, parse_frame, recv_frame_blocking,
};

#[test]
fn stream_receiver_preserves_back_to_back_frames() {
    let (mut writer, reader) = UnixStream::pair().expect("local stream pair");
    let mut first = [0u8; 24];
    let first = Accepted { revision: 7 }
        .encode(&mut first)
        .expect("accepted frame");
    let mut second = [0u8; 48];
    let second = Presented {
        revision: 7,
        frame_sequence: 11,
        monotonic_ns: 13,
    }
    .encode(&mut second)
    .expect("presented frame");
    let mut coalesced = Vec::from(first);
    coalesced.extend_from_slice(second);
    writer
        .write_all(&coalesced)
        .expect("both frames must enter one stream write");

    let mut bytes = [0u8; 64];
    let (length, fd) = recv_frame_blocking(&reader, &mut bytes).expect("first frame");
    assert!(fd.is_none());
    assert_eq!(
        parse_frame(&bytes[..length]).expect("first parse").kind(),
        MessageKind::Accepted
    );
    let (length, fd) = recv_frame_blocking(&reader, &mut bytes).expect("second frame");
    assert!(fd.is_none());
    assert_eq!(
        parse_frame(&bytes[..length]).expect("second parse").kind(),
        MessageKind::Presented
    );
}

#[test]
fn handshake_requires_exact_version_and_frame_length() {
    let mut bytes = [0u8; 128];
    let encoded = HelloApp {
        version: PROTOCOL_VERSION,
        app_id: b"terminal",
    }
    .encode(&mut bytes)
    .expect("valid handshake must encode");
    let frame = parse_frame(encoded).expect("complete frame must parse");
    assert_eq!(frame.kind(), MessageKind::HelloApp);
    let hello = HelloApp::parse(frame.payload()).expect("exact version must parse");
    assert_eq!(hello.app_id, b"terminal");

    let mut with_trailing = encoded.to_vec();
    with_trailing.push(0);
    assert!(parse_frame(&with_trailing).is_none());

    let mut wrong_version = encoded.to_vec();
    wrong_version[8..12].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    let frame = parse_frame(&wrong_version).expect("wire frame remains structurally valid");
    assert!(HelloApp::parse(frame.payload()).is_none());
}

#[test]
fn lifecycle_and_input_preserve_exact_surface_routing() {
    let mut bytes = [0u8; 128];
    let opened = AppOpened {
        surface_id: 9,
        app_id: b"terminal",
    }
    .encode(&mut bytes)
    .expect("opened message must encode");
    let frame = parse_frame(opened).expect("opened frame must parse");
    let opened = AppOpened::parse(frame.payload()).expect("opened payload must parse");
    assert_eq!(
        (opened.surface_id, opened.app_id),
        (9, b"terminal".as_slice())
    );

    let pointer = InputPointer {
        surface_id: 9,
        serial: 44,
        phase: PointerPhase::Down,
        button: 272,
        buttons: 1,
        x: 17,
        y: 23,
    }
    .encode(&mut bytes)
    .expect("pointer message must encode");
    let frame = parse_frame(pointer).expect("pointer frame must parse");
    assert_eq!(
        InputPointer::parse(frame.payload()).expect("pointer payload"),
        InputPointer {
            surface_id: 9,
            serial: 44,
            phase: PointerPhase::Down,
            button: 272,
            buttons: 1,
            x: 17,
            y: 23,
        }
    );

    let key = InputKey {
        surface_id: 9,
        code: 30,
        value: 1,
        modifiers: 1,
    }
    .encode(&mut bytes)
    .expect("key message must encode");
    let frame = parse_frame(key).expect("key frame must parse");
    assert_eq!(
        InputKey::parse(frame.payload()).expect("key payload").code,
        30
    );
}

#[test]
fn allocation_accepts_only_single_or_double_buffer() {
    let mut bytes = [0u8; 128];
    for count in [1, 2] {
        let encoded = BufferAlloc {
            request_id: 7,
            size: Size {
                width: 640,
                height: 480,
            },
            count,
        }
        .encode(&mut bytes)
        .expect("supported count must encode");
        let frame = parse_frame(encoded).expect("allocation frame must parse");
        assert_eq!(
            BufferAlloc::parse(frame.payload())
                .expect("allocation payload must parse")
                .count,
            count
        );
    }
    assert!(
        BufferAlloc {
            request_id: 7,
            size: Size {
                width: 640,
                height: 480,
            },
            count: 3,
        }
        .encode(&mut bytes)
        .is_none()
    );
}

#[test]
fn surface_damage_round_trips_without_native_layout_casts() {
    let damage = [
        Rect {
            x: 3,
            y: 4,
            width: 10,
            height: 12,
        },
        Rect {
            x: 20,
            y: 30,
            width: 2,
            height: 5,
        },
    ];
    let mut bytes = [0u8; 256];
    let encoded = SurfaceCommit::encode(&mut bytes, 11, 9, 4, &damage)
        .expect("bounded surface commit must encode");
    let frame = parse_frame(encoded).expect("surface frame must parse");
    let commit = SurfaceCommit::parse(frame.payload()).expect("surface payload must parse");
    assert_eq!(commit.damage().collect::<Vec<_>>(), damage);
}

#[test]
fn scene_round_trips_variable_regions_and_node_kinds() {
    let input = [Rect {
        x: 4,
        y: 5,
        width: 100,
        height: 20,
    }];
    let damage = [Rect {
        x: 0,
        y: 0,
        width: 300,
        height: 200,
    }];
    let nodes = [SceneNode {
        kind: SceneNodeKind::Pixels,
        window_group: 8,
        source_id: 14,
        configure_serial: 0,
        bounds: damage[0],
        clip: damage[0],
        opaque: None,
        input: Rectangles::from_slice(&input),
        damage: Rectangles::from_slice(&damage),
    }];
    let mut bytes = [0u8; 512];
    let encoded =
        SceneCommit::encode(&mut bytes, 22, 8, &nodes).expect("bounded scene must encode");
    let frame = parse_frame(encoded).expect("scene frame must parse");
    let scene = SceneCommit::parse(frame.payload()).expect("scene payload must validate fully");
    let parsed = scene.nodes().next().expect("one node must remain");
    assert_eq!(parsed.kind, SceneNodeKind::Pixels);
    assert_eq!(parsed.input.iter().collect::<Vec<_>>(), input);
    assert_eq!(parsed.damage.iter().collect::<Vec<_>>(), damage);
}
