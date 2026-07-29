use std::{io::Write, os::unix::net::UnixStream};

use display_proto::{
    AcceleratorChord, AcceleratorSet, Accepted, AppOpened, BufferAlloc, CURSOR_DEFAULT,
    CURSOR_NONE, CURSOR_RESIZE_NWSE, ClipboardData, ClipboardRead, ClipboardWrite, HelloApp,
    InputKey, InputPointer, InputScroll, MAX_ACCELERATORS, MAX_CLIPBOARD_TEXT, MAX_MESSAGE,
    MessageKind, MoveBegin, MoveComplete, PROTOCOL_VERSION, PointerPhase, Presented, Rect,
    Rectangles, SceneCommit, SceneNode, SceneNodeKind, SetCursorShape, Size, SurfaceCommit,
    parse_frame, recv_frame_blocking,
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
fn move_grab_round_trips_authority_constraints_and_signed_result() {
    let mut bytes = [0u8; 128];
    let begin = MoveBegin {
        surface_id: 9,
        serial: 44,
        underlay_buffer_id: 17,
        min_x: -120,
        min_y: 0,
        max_x: 1504,
        max_y: 846,
    };
    let encoded = begin
        .encode(&mut bytes)
        .expect("valid move authorization must encode");
    let frame = parse_frame(encoded).expect("move-begin frame must parse");
    assert_eq!(frame.kind(), MessageKind::MoveBegin);
    assert_eq!(
        MoveBegin::parse(frame.payload()).expect("move-begin payload"),
        begin
    );

    let complete = MoveComplete {
        surface_id: 9,
        x: -48,
        y: 27,
    };
    let encoded = complete
        .encode(&mut bytes)
        .expect("valid move result must encode");
    let frame = parse_frame(encoded).expect("move-complete frame must parse");
    assert_eq!(frame.kind(), MessageKind::MoveComplete);
    assert_eq!(
        MoveComplete::parse(frame.payload()).expect("move-complete payload"),
        complete
    );

    assert!(
        MoveBegin {
            max_x: -121,
            ..begin
        }
        .encode(&mut bytes)
        .is_none()
    );
    assert!(
        MoveBegin {
            underlay_buffer_id: 0,
            ..begin
        }
        .encode(&mut bytes)
        .is_none()
    );
}

#[test]
fn scroll_round_trips_surface_local_coordinates_and_signed_deltas() {
    let mut bytes = [0u8; 64];
    for event in [
        InputScroll {
            surface_id: 0,
            serial: 7,
            x: 12,
            y: 34,
            delta_x: 0,
            delta_y: -3,
        },
        InputScroll {
            surface_id: 9,
            serial: 88,
            x: 5,
            y: 6,
            delta_x: -2,
            delta_y: 4,
        },
    ] {
        let encoded = event
            .encode(&mut bytes)
            .expect("scroll message must encode");
        let frame = parse_frame(encoded).expect("scroll frame must parse");
        assert_eq!(frame.kind(), MessageKind::InputScroll);
        assert_eq!(
            InputScroll::parse(frame.payload()).expect("scroll payload"),
            event
        );
    }
}

#[test]
fn clipboard_round_trips_utf8_identity_and_rejects_oversize() {
    let mut bytes = vec![0u8; MAX_MESSAGE];
    let read = ClipboardRead {
        surface_id: 9,
        request_id: 44,
    };
    let frame = parse_frame(read.encode(&mut bytes).expect("clipboard read must encode"))
        .expect("clipboard read frame");
    assert_eq!(frame.kind(), MessageKind::ClipboardRead);
    assert_eq!(
        ClipboardRead::parse(frame.payload()).expect("clipboard read payload"),
        read
    );

    let write = ClipboardWrite {
        surface_id: 9,
        text: "来自 macOS 的文本".to_owned(),
    };
    let frame = parse_frame(
        write
            .encode(&mut bytes)
            .expect("clipboard write must encode"),
    )
    .expect("clipboard write frame");
    assert_eq!(
        ClipboardWrite::parse(frame.payload()).expect("clipboard write payload"),
        write
    );

    let data = ClipboardData {
        surface_id: 9,
        request_id: 44,
        text: "LiteOS".to_owned(),
    };
    let frame = parse_frame(data.encode(&mut bytes).expect("clipboard data must encode"))
        .expect("clipboard data frame");
    assert_eq!(
        ClipboardData::parse(frame.payload()).expect("clipboard data payload"),
        data
    );

    let oversized = ClipboardWrite {
        surface_id: 9,
        text: "x".repeat(MAX_CLIPBOARD_TEXT + 1),
    };
    assert!(oversized.encode(&mut bytes).is_none());
}

#[test]
fn set_cursor_shape_round_trips_surface_and_shape() {
    let mut bytes = [0u8; 64];
    for request in [
        SetCursorShape {
            surface_id: 0,
            shape: CURSOR_DEFAULT,
        },
        SetCursorShape {
            surface_id: 9,
            shape: CURSOR_RESIZE_NWSE,
        },
        SetCursorShape {
            surface_id: 0,
            shape: CURSOR_NONE,
        },
    ] {
        let encoded = request
            .encode(&mut bytes)
            .expect("cursor-shape request must encode");
        let frame = parse_frame(encoded).expect("cursor-shape frame must parse");
        assert_eq!(frame.kind(), MessageKind::SetCursorShape);
        assert_eq!(
            SetCursorShape::parse(frame.payload()).expect("cursor-shape payload"),
            request
        );
    }
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
        corner_radius: 0,
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

#[test]
fn accelerator_set_round_trips_bounded_chords() {
    let chords = [
        AcceleratorChord {
            modifiers: 4,
            code: 15,
        },
        AcceleratorChord {
            modifiers: 2,
            code: 46,
        },
    ];
    let mut bytes = [0u8; 64];
    let encoded = AcceleratorSet { chords: &chords }
        .encode(&mut bytes)
        .expect("bounded table must encode");
    let frame = parse_frame(encoded).expect("accelerator frame must parse");
    assert_eq!(frame.kind(), MessageKind::AcceleratorSet);
    let parsed = AcceleratorSet::parse(frame.payload()).expect("table payload must parse");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed.collect::<Vec<_>>(), chords);
}

#[test]
fn accelerator_set_empty_table_clears_chords() {
    let mut bytes = [0u8; 16];
    let encoded = AcceleratorSet { chords: &[] }
        .encode(&mut bytes)
        .expect("empty table must encode");
    let frame = parse_frame(encoded).expect("accelerator frame must parse");
    let parsed = AcceleratorSet::parse(frame.payload()).expect("empty payload must parse");
    assert_eq!(parsed.len(), 0);
    assert_eq!(parsed.count(), 0);
}

#[test]
fn accelerator_set_rejects_over_limit_tables() {
    let chords = [AcceleratorChord {
        modifiers: 0,
        code: 1,
    }; MAX_ACCELERATORS + 1];
    let mut bytes = [0u8; 256];
    assert!(
        AcceleratorSet { chords: &chords }
            .encode(&mut bytes)
            .is_none()
    );

    // A peer could still place an oversized count on the wire; the decoder
    // must reject it even when the chord bytes themselves are present.
    let mut payload = Vec::from((MAX_ACCELERATORS as u32 + 1).to_le_bytes());
    payload.extend_from_slice(&[0u8; (MAX_ACCELERATORS + 1) * 8]);
    assert!(AcceleratorSet::parse(&payload).is_none());
}

#[test]
fn accelerator_set_rejects_truncated_and_overlong_payloads() {
    let chords = [AcceleratorChord {
        modifiers: 4,
        code: 15,
    }];
    let mut bytes = [0u8; 32];
    let encoded = AcceleratorSet { chords: &chords }
        .encode(&mut bytes)
        .expect("one chord must encode");
    let frame = parse_frame(encoded).expect("frame must parse");
    let payload = frame.payload();
    assert!(AcceleratorSet::parse(&payload[..payload.len() - 1]).is_none());

    let mut overlong = Vec::from(payload);
    overlong.extend_from_slice(&[0u8; 8]);
    assert!(AcceleratorSet::parse(&overlong).is_none());
}
