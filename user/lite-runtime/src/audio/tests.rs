use std::{
    collections::VecDeque,
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use audio_proto::{
    AckOperation, CHANNELS, ClientRole, ConsumerRing, MAX_FRAME_LEN, MappedProducer,
    RING_CAPACITY_FRAMES, RING_MAPPING_BYTES, ServiceMessage, encode_service, initialize_ring,
    send_service,
};
use linux_uapi::shared_memory::{MemFd, SharedMapping};

use super::{
    Command, DecoderSession, Media, PREFILL_BLOCKS, RENDER_FRAMES, UnixStream, Worker,
    interleaved_frames, quantum_fits,
};

#[test]
fn stereo_frame_projection_preserves_channel_order() {
    let samples = [0.25, -0.5, 0.75, 1.0];
    assert_eq!(interleaved_frames(&samples), &[[0.25, -0.5], [0.75, 1.0]]);
}

#[test]
fn initial_prefill_reaches_the_ring_backpressure_edge() {
    assert_eq!(PREFILL_BLOCKS * RENDER_FRAMES, RING_CAPACITY_FRAMES);
}

#[test]
fn long_playback_refills_exactly_one_device_period_without_overfill() {
    let mut produced = RING_CAPACITY_FRAMES as u64;
    let mut consumed = 0u64;
    assert!(!quantum_fits(produced, consumed).expect("full ring"));
    for _ in 0..11_250 {
        consumed += 256;
        assert!(quantum_fits(produced, consumed).expect("first quantum"));
        produced += RENDER_FRAMES as u64;
        assert!(quantum_fits(produced, consumed).expect("second quantum"));
        produced += RENDER_FRAMES as u64;
        assert!(!quantum_fits(produced, consumed).expect("backpressure"));
    }
}

#[test]
fn one_delayed_ring_available_edge_refills_every_consumed_period() {
    let (command_wake, _command_notifier) = UnixStream::pair().unwrap();
    let (event_wake, _event_notifier) = UnixStream::pair().unwrap();
    let mut worker = Worker::new(
        ClientRole::Media,
        Arc::new(Mutex::new(VecDeque::new())),
        command_wake,
        Arc::new(Mutex::new(VecDeque::new())),
        event_wake,
    );
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/audio/tone.wav");
    let length = path.metadata().unwrap().len();
    let decoder = DecoderSession::open_range(&path, 0, length).unwrap();
    let generation = 3;
    let stream_id = 11;
    let memfd = MemFd::create("lite-ui-refill-test", RING_MAPPING_BYTES).unwrap();
    let mapping = SharedMapping::map(memfd.as_fd(), RING_MAPPING_BYTES).unwrap();
    // SAFETY: The new mapping is initialized before either unique SPSC view is projected.
    unsafe { initialize_ring(mapping.as_non_null(), mapping.len(), generation) }.unwrap();
    let mut consumer =
        unsafe { ConsumerRing::from_mapping(mapping.as_non_null(), mapping.len()) }.unwrap();
    let (service, client) = UnixStream::pair().unwrap();
    let producer = MappedProducer::map(memfd.as_fd().try_clone_to_owned().unwrap()).unwrap();
    worker.service = Some(client);
    worker.media.insert(
        7,
        Media {
            decoder,
            generation,
            stream_id: Some(stream_id),
            ring: Some(producer),
            playing: true,
            starting: false,
            ending: false,
            loop_enabled: false,
            gain: 1.0,
            base_seconds: 0.0,
            submitted_frames: 0,
            consumed_frames: 0,
            decode_eof: false,
            last_timeupdate: Instant::now(),
            resume_position: None,
        },
    );
    worker.prefill(7).unwrap();
    let initial = worker.media[&7].ring.as_ref().unwrap().snapshot().unwrap();
    assert_eq!(
        initial.produced_frames - initial.consumed_frames,
        RING_CAPACITY_FRAMES as u64
    );

    let mut output = [[0.0; CHANNELS]; 256];
    // Periods to drain a full ring down across the refill watermark. Consumption
    // is `output.len()` frames/period; the crossing fires once at the boundary.
    let cross_periods =
        (RING_CAPACITY_FRAMES - audio_proto::LOW_WATER_FRAMES).div_ceil(output.len());
    // Repeat the drain→refill cycle: each cycle must produce exactly one edge and
    // one edge must restore the ring to full, proving sustained playback re-arms.
    for _ in 0..4 {
        let mut available_edges = 0;
        for _ in 0..cross_periods {
            let (consumed, became_available) =
                consumer.mix_into(generation, 1.0, &mut output).unwrap();
            assert_eq!(consumed, output.len());
            available_edges += usize::from(became_available);
        }
        assert_eq!(
            available_edges, 1,
            "draining across the low watermark must coalesce to one refill edge"
        );
        let before_refill = worker.media[&7].ring.as_ref().unwrap().snapshot().unwrap();
        send_service(
            &service,
            ServiceMessage::RingAvailable {
                stream_id,
                generation: generation - 1,
            },
            None,
        )
        .unwrap();
        worker.receive_service().unwrap();
        assert_eq!(
            worker.media[&7].ring.as_ref().unwrap().snapshot().unwrap(),
            before_refill,
            "a stale generation must not refill the active ring"
        );

        send_service(
            &service,
            ServiceMessage::RingAvailable {
                stream_id,
                generation,
            },
            None,
        )
        .unwrap();
        worker.receive_service().unwrap();
        let refilled = worker.media[&7].ring.as_ref().unwrap().snapshot().unwrap();
        assert_eq!(refilled.generation, generation);
        assert_eq!(
            refilled.produced_frames - refilled.consumed_frames,
            RING_CAPACITY_FRAMES as u64,
            "one refill edge must restore the shared ring to full"
        );
    }
}

#[test]
fn create_barrier_routes_old_close_ack_before_validating_stream_descriptor() {
    let (command_wake, _command_notifier) = UnixStream::pair().unwrap();
    let (event_wake, _event_notifier) = UnixStream::pair().unwrap();
    let (mut service, client) = UnixStream::pair().unwrap();
    let mut worker = Worker::new(
        ClientRole::Media,
        Arc::new(Mutex::new(VecDeque::new())),
        command_wake,
        Arc::new(Mutex::new(VecDeque::new())),
        event_wake,
    );
    worker.service = Some(client);
    worker.closing_streams.insert(41, 9);
    send_service(
        &service,
        ServiceMessage::Ack {
            stream_id: 41,
            generation: 9,
            operation: AckOperation::Close,
        },
        None,
    )
    .unwrap();
    let mut bytes = [0; MAX_FRAME_LEN];
    let created = encode_service(
        ServiceMessage::StreamCreated {
            stream_id: 42,
            generation: 1,
            capacity_frames: RING_CAPACITY_FRAMES as u32,
        },
        &mut bytes,
    )
    .unwrap();
    service.write_all(created.as_bytes()).unwrap();

    let error = match worker.wait_for_stream_created(1) {
        Err(error) => error,
        Ok(_) => panic!("descriptorless stream creation was accepted"),
    };
    assert_eq!(error, "invalid audio service descriptor publication");
    assert!(
        worker.closing_streams.is_empty(),
        "old Close acknowledgement was not routed before the create reply"
    );
}

#[test]
fn create_barrier_rejects_unknown_async_stream() {
    let (command_wake, _command_notifier) = UnixStream::pair().unwrap();
    let (event_wake, _event_notifier) = UnixStream::pair().unwrap();
    let (service, client) = UnixStream::pair().unwrap();
    let mut worker = Worker::new(
        ClientRole::Media,
        Arc::new(Mutex::new(VecDeque::new())),
        command_wake,
        Arc::new(Mutex::new(VecDeque::new())),
        event_wake,
    );
    worker.service = Some(client);
    send_service(
        &service,
        ServiceMessage::Ack {
            stream_id: 99,
            generation: 7,
            operation: AckOperation::Close,
        },
        None,
    )
    .unwrap();
    let error = match worker.wait_for_stream_created(1) {
        Err(error) => error,
        Ok(_) => panic!("unknown asynchronous stream was accepted"),
    };
    assert_eq!(error, "audio service violated create-stream ordering");
}

#[cfg(target_os = "linux")]
#[test]
fn real_socket_create_barrier_accepts_old_close_ack_before_stream_created() {
    let (command_wake, _command_notifier) = UnixStream::pair().unwrap();
    let (event_wake, _event_notifier) = UnixStream::pair().unwrap();
    let (service, client) = UnixStream::pair().unwrap();
    let mut worker = Worker::new(
        ClientRole::Media,
        Arc::new(Mutex::new(VecDeque::new())),
        command_wake,
        Arc::new(Mutex::new(VecDeque::new())),
        event_wake,
    );
    worker.service = Some(client);
    worker.closing_streams.insert(41, 9);
    let memfd = MemFd::create("lite-ui-create-barrier-test", RING_MAPPING_BYTES).unwrap();
    let mapping = SharedMapping::map(memfd.as_fd(), RING_MAPPING_BYTES).unwrap();
    // SAFETY: The service-owned mapping is initialized before its descriptor is published.
    unsafe { initialize_ring(mapping.as_non_null(), mapping.len(), 1) }.unwrap();
    send_service(
        &service,
        ServiceMessage::Ack {
            stream_id: 41,
            generation: 9,
            operation: AckOperation::Close,
        },
        None,
    )
    .unwrap();
    send_service(
        &service,
        ServiceMessage::StreamCreated {
            stream_id: 42,
            generation: 1,
            capacity_frames: RING_CAPACITY_FRAMES as u32,
        },
        Some(memfd.as_fd()),
    )
    .unwrap();

    let (stream_id, ring) = worker.wait_for_stream_created(1).unwrap();
    assert_eq!(stream_id, 42);
    assert_eq!(ring.mapping_len(), RING_MAPPING_BYTES);
    assert!(worker.closing_streams.is_empty());
}

#[test]
fn loaded_media_accepts_loop_changes_and_close_without_retiring_element_identity() {
    let (command_wake, _command_notifier) = UnixStream::pair().unwrap();
    let (event_wake, _event_notifier) = UnixStream::pair().unwrap();
    let mut worker = Worker::new(
        ClientRole::Media,
        Arc::new(Mutex::new(VecDeque::new())),
        command_wake,
        Arc::new(Mutex::new(VecDeque::new())),
        event_wake,
    );
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/audio/tone.wav");
    let length = path.metadata().unwrap().len();
    worker
        .handle(Command::Load {
            id: 7,
            path,
            offset: 0,
            length,
            stream: None,
        })
        .unwrap();
    assert!(worker.media.contains_key(&7));
    worker
        .handle(Command::Loop {
            id: 7,
            enabled: true,
        })
        .unwrap();
    assert!(worker.media.get(&7).unwrap().loop_enabled);
    worker
        .handle(Command::Loop {
            id: 7,
            enabled: false,
        })
        .unwrap();
    assert!(!worker.media.get(&7).unwrap().loop_enabled);
    worker.handle(Command::Close { id: 7 }).unwrap();
    assert!(!worker.media.contains_key(&7));
}
