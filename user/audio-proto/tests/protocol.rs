use audio_proto::{
    AckOperation, ClientMessage, ClientRole, ErrorCode, MAX_FRAME_LEN, PROTOCOL_VERSION,
    ProtocolError, ServiceMessage, decode_client, decode_service, encode_client, encode_service,
};

#[test]
fn every_client_message_round_trips() {
    let messages = [
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            role: ClientRole::Media,
        },
        ClientMessage::CreateStream { generation: 1 },
        ClientMessage::Start {
            stream_id: 2,
            generation: 3,
        },
        ClientMessage::Pause {
            stream_id: 2,
            generation: 3,
        },
        ClientMessage::Flush {
            stream_id: 2,
            old_generation: 3,
            new_generation: 4,
        },
        ClientMessage::SetGain {
            stream_id: 2,
            generation: 4,
            gain: 0.375,
        },
        ClientMessage::Close {
            stream_id: 2,
            generation: 4,
        },
        ClientMessage::RingNonempty {
            stream_id: 2,
            generation: 4,
        },
        ClientMessage::GetMasterState,
        ClientMessage::SetMasterVolume { percent: 75 },
        ClientMessage::SetMasterMuted { muted: true },
    ];
    for message in messages {
        let mut bytes = [0; MAX_FRAME_LEN];
        let frame = encode_client(message, &mut bytes).expect("encode");
        assert_eq!(decode_client(frame.as_bytes()), Ok(message));
    }
}

#[test]
fn every_service_message_round_trips() {
    let messages = [
        ServiceMessage::Welcome {
            version: PROTOCOL_VERSION,
        },
        ServiceMessage::StreamCreated {
            stream_id: 1,
            generation: 2,
            capacity_frames: 8192,
        },
        ServiceMessage::Ack {
            stream_id: 1,
            generation: 2,
            operation: AckOperation::Gain,
        },
        ServiceMessage::Flushed {
            stream_id: 1,
            generation: 3,
        },
        ServiceMessage::Progress {
            stream_id: 1,
            generation: 3,
            consumed_frames: 4096,
            concurrent_playbacks: 8,
        },
        ServiceMessage::RingAvailable {
            stream_id: 1,
            generation: 3,
        },
        ServiceMessage::Error {
            stream_id: Some(1),
            generation: 3,
            code: ErrorCode::CorruptRing,
        },
        ServiceMessage::Error {
            stream_id: None,
            generation: 0,
            code: ErrorCode::ProtocolMismatch,
        },
        ServiceMessage::MasterState {
            percent: 75,
            muted: false,
        },
    ];
    for message in messages {
        let mut bytes = [0; MAX_FRAME_LEN];
        let frame = encode_service(message, &mut bytes).expect("encode");
        assert_eq!(decode_service(frame.as_bytes()), Ok(message));
    }
}

#[test]
fn decoder_rejects_oversize_unknown_and_trailing_payload() {
    let mut oversized = vec![0; MAX_FRAME_LEN + 1];
    let oversized_len = oversized.len() as u32;
    oversized[..4].copy_from_slice(&oversized_len.to_le_bytes());
    oversized[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(decode_client(&oversized), Err(ProtocolError::InvalidLength));

    let mut unknown = [0; 8];
    unknown[..4].copy_from_slice(&8u32.to_le_bytes());
    unknown[4..].copy_from_slice(&999u32.to_le_bytes());
    assert_eq!(decode_client(&unknown), Err(ProtocolError::UnknownMessage));

    let mut bytes = [0; MAX_FRAME_LEN];
    let frame = encode_client(ClientMessage::GetMasterState, &mut bytes)
        .expect("encode")
        .as_bytes()
        .to_vec();
    let mut trailing = frame;
    trailing.push(0);
    let len = trailing.len() as u32;
    trailing[..4].copy_from_slice(&len.to_le_bytes());
    assert_eq!(decode_client(&trailing), Err(ProtocolError::InvalidPayload));
}
