use std::{
    io::{self, Read},
    os::fd::AsFd,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use audio_proto::{
    CHANNELS, ConsumerRing, ProducerRing, RING_CAPACITY_FRAMES, RING_MAPPING_BYTES, ServiceMessage,
    initialize_ring, recv_service,
};
use linux_uapi::shared_memory::{MemFd, SharedMapping};

use super::*;
use crate::{
    mixer::{DeviceError, Mixer, PERIOD_FRAMES, StreamMemory},
    queue::SpscQueue,
};

struct DeviceState {
    writes: AtomicUsize,
    permits: Mutex<usize>,
    ready: Condvar,
}

struct MockDevice(Arc<DeviceState>);

impl PlaybackDevice for MockDevice {
    fn activate(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }

    fn wait_period(&mut self) -> Result<(), DeviceError> {
        let mut permits = self.0.permits.lock().expect("period permits");
        while *permits == 0 {
            permits = self.0.ready.wait(permits).expect("period wait");
        }
        *permits -= 1;
        Ok(())
    }

    fn write_period(
        &mut self,
        _frames: &[[f32; CHANNELS]; PERIOD_FRAMES],
    ) -> Result<bool, DeviceError> {
        self.0.writes.fetch_add(1, Ordering::Release);
        Ok(false)
    }

    fn delay_frames(&mut self) -> Result<u32, DeviceError> {
        Ok(0)
    }

    fn recover_xrun(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }

    fn stop(&mut self) {}
}

fn mapped_ring(generation: u64) -> (StreamMemory, ProducerRing) {
    let memfd = MemFd::create("audio-service-edge-test", RING_MAPPING_BYTES).expect("memfd");
    let mapping = SharedMapping::map(memfd.as_fd(), RING_MAPPING_BYTES).expect("mapping");
    // SAFETY: The new mapping is exclusively initialized before producer/consumer projection.
    unsafe { initialize_ring(mapping.as_non_null(), mapping.len(), generation) }
        .expect("initialize ring");
    let producer = unsafe { ProducerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
        .expect("producer");
    let consumer = unsafe { ConsumerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
        .expect("consumer");
    (StreamMemory::new(mapping, consumer), producer)
}

fn grant_periods(state: &DeviceState, count: usize) {
    *state.permits.lock().expect("period permits") += count;
    state.ready.notify_one();
}

fn wait_for_writes(state: &DeviceState, expected: usize) {
    while state.writes.load(Ordering::Acquire) < expected {
        thread::yield_now();
    }
}

#[test]
fn active_service_socket_is_not_unlinked_by_second_instance() {
    let path = std::env::temp_dir().join(format!(
        "liteos-audio-socket-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let (listener, guard) = bind_listener(&path).expect("first listener");
    let error = match bind_listener(&path) {
        Ok(_) => panic!("second listener unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(path.exists());
    drop(listener);
    drop(guard);
    assert!(!path.exists());
}

#[test]
fn low_watermark_crossing_arms_and_rearms_ring_available() {
    // Periods to drain a full ring down across LOW_WATER_FRAMES: consumption is
    // PERIOD_FRAMES/period, and the crossing fires when `available` first moves
    // to at-or-below the watermark. 8192 → 4096 is exactly 16 periods.
    const CROSS_PERIODS: usize =
        (RING_CAPACITY_FRAMES - audio_proto::LOW_WATER_FRAMES) / PERIOD_FRAMES;
    let commands = Arc::new(SpscQueue::new());
    let events = Arc::new(SpscQueue::new());
    let failed = Arc::new(AtomicBool::new(false));
    let device = Arc::new(DeviceState {
        writes: AtomicUsize::new(0),
        permits: Mutex::new(0),
        ready: Condvar::new(),
    });
    let (memory, mut producer) = mapped_ring(1);
    assert_eq!(
        producer.write(1, &vec![[0.25, -0.25]; RING_CAPACITY_FRAMES]),
        Ok((RING_CAPACITY_FRAMES, true))
    );
    assert!(
        commands
            .push(MixerCommand::Add {
                stream_id: 1,
                generation: 1,
                gain: 1.0,
                memory,
            })
            .is_ok()
    );
    assert!(
        commands
            .push(MixerCommand::Start {
                stream_id: 1,
                generation: 1,
            })
            .is_ok()
    );

    let (event_reader, event_writer) = UnixStream::pair().expect("event wake");
    event_reader
        .set_nonblocking(true)
        .expect("nonblocking wake");
    event_writer
        .set_nonblocking(true)
        .expect("nonblocking wake");
    let mixer = Mixer::new(
        MockDevice(device.clone()),
        commands.clone(),
        events.clone(),
        event_writer,
        failed.clone(),
        1.0,
    );
    let mixer_thread = thread::spawn(move || mixer.run());

    // Drain from full down to the watermark; the last period crosses it once.
    grant_periods(&device, CROSS_PERIODS);
    wait_for_writes(&device, CROSS_PERIODS);
    assert!(!failed.load(Ordering::Acquire));

    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("host clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "liteos-audio-service-edge-{}-{unique}",
        std::process::id()
    ));
    let (listener, socket_guard) = bind_listener(&path).expect("listener");
    let (service_socket, client_socket) = UnixStream::pair().expect("client socket");
    service_socket
        .set_nonblocking(true)
        .expect("service nonblocking");
    client_socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("client timeout");
    let settings_path = path.with_extension("settings");
    let mut service = Service::<MockDevice> {
        listener,
        _socket_guard: socket_guard,
        event_wake: event_reader,
        connections: vec![Connection {
            identity: 1,
            pid: std::process::id() as i32,
            stream: service_socket,
            receiver: ClientFrameReceiver::new(),
            role: Some(ClientRole::Media),
        }],
        streams: vec![StreamRecord {
            connection: 1,
            pid: std::process::id() as i32,
            id: 1,
            generation: 1,
            phase: StreamPhase::Live,
            confirmed_frames: 0,
            next_progress_marker: 1,
        }],
        next_connection: 2,
        next_stream: 2,
        settings: Settings::load(settings_path).expect("settings"),
        commands: commands.clone(),
        events: events.clone(),
        mixer_failed: failed,
        mixer: None,
        _device: std::marker::PhantomData,
    };

    // Exactly one RingAvailable reaches the client from the single crossing;
    // Progress frames may accompany it, so drain everything and count edges.
    service.drain_event_wake().expect("drain mixer wake");
    assert!(!service.drain_mixer_events().expect("route mixer events"));
    let mut frame = [0; audio_proto::MAX_FRAME_LEN];
    let mut first_edge = 0;
    client_socket
        .set_nonblocking(true)
        .expect("nonblocking client");
    loop {
        match recv_service(&client_socket, &mut frame) {
            Ok(Some((message, fd))) => {
                assert!(fd.is_none());
                first_edge += usize::from(matches!(
                    message,
                    ServiceMessage::RingAvailable {
                        stream_id: 1,
                        generation: 1
                    }
                ));
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("service frame: {error}"),
        }
    }
    client_socket
        .set_nonblocking(false)
        .expect("blocking client");
    assert_eq!(first_edge, 1);

    // A refill that keeps `available` strictly above the watermark does NOT
    // re-arm — the edge fires only on a fresh downward crossing, so a steady
    // drain emits exactly one request per refill cycle (no notification storm).
    // Write two periods then consume one, so the level stays above the line.
    assert_eq!(
        producer.write(1, &[[0.5, -0.5]; 2 * PERIOD_FRAMES]),
        Ok((2 * PERIOD_FRAMES, false))
    );
    grant_periods(&device, 1);
    wait_for_writes(&device, CROSS_PERIODS + 1);
    service
        .drain_event_wake()
        .expect("drain partial-refill wake");
    assert!(
        !service
            .drain_mixer_events()
            .expect("route partial-refill events")
    );
    client_socket
        .set_nonblocking(true)
        .expect("nonblocking client");
    // Progress frames still flow (they track playback time); only assert that no
    // RingAvailable re-fires while the level stays above the watermark.
    loop {
        match recv_service(&client_socket, &mut frame) {
            Ok(Some((message, fd))) => {
                assert!(fd.is_none());
                assert!(
                    !matches!(
                        message,
                        ServiceMessage::RingAvailable {
                            stream_id: 1,
                            generation: 1
                        }
                    ),
                    "staying above the watermark must not re-fire the crossing",
                );
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("partial-refill service frame: {error}"),
        }
    }
    client_socket
        .set_nonblocking(false)
        .expect("blocking client");

    // Refill a little above the watermark, then drain across it again: the
    // crossing re-arms and RingAvailable fires once more — the recovery the
    // exactly-full edge could never provide. Level here is 4352 (4608 written
    // minus one consumed period); +256 → 4608, and two periods drain to 4096,
    // re-crossing the watermark exactly once.
    assert_eq!(
        producer.write(1, &[[0.5, -0.5]; PERIOD_FRAMES]),
        Ok((PERIOD_FRAMES, false))
    );
    grant_periods(&device, 2);
    wait_for_writes(&device, CROSS_PERIODS + 1 + 2);
    service.drain_event_wake().expect("drain rearmed wake");
    assert!(!service.drain_mixer_events().expect("route rearmed edge"));
    let mut rearmed_edge = 0;
    client_socket
        .set_nonblocking(true)
        .expect("nonblocking drain");
    loop {
        match recv_service(&client_socket, &mut frame) {
            Ok(Some((message, fd))) => {
                assert!(fd.is_none());
                rearmed_edge += usize::from(matches!(
                    message,
                    ServiceMessage::RingAvailable {
                        stream_id: 1,
                        generation: 1
                    }
                ));
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("rearmed service frame: {error}"),
        }
    }
    assert_eq!(rearmed_edge, 1);

    assert!(commands.push(MixerCommand::Shutdown).is_ok());
    grant_periods(&device, 1);
    mixer_thread.join().expect("mixer");
    let mut wake = [0; 8];
    let _ = service.event_wake.read(&mut wake);
}
