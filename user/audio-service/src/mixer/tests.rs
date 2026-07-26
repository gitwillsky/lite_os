use std::{
    os::fd::AsFd,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize},
    },
    thread,
};

use audio_proto::{ProducerRing, RING_CAPACITY_FRAMES, RING_MAPPING_BYTES, initialize_ring};

use super::*;

struct DeviceState {
    activations: AtomicUsize,
    writes: AtomicUsize,
    stops: AtomicUsize,
    last: Mutex<[[f32; CHANNELS]; PERIOD_FRAMES]>,
    permits: Mutex<usize>,
    ready: Condvar,
    starting: AtomicBool,
}

struct MockDevice(Arc<DeviceState>);

impl PlaybackDevice for MockDevice {
    fn activate(&mut self) -> Result<(), DeviceError> {
        self.0.activations.fetch_add(1, Ordering::Relaxed);
        self.0.starting.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn wait_period(&mut self) -> Result<(), DeviceError> {
        let mut permits = self.0.permits.lock().expect("permits");
        while *permits == 0 {
            permits = self.0.ready.wait(permits).expect("period wait");
        }
        *permits -= 1;
        Ok(())
    }

    fn write_period(
        &mut self,
        frames: &[[f32; CHANNELS]; PERIOD_FRAMES],
    ) -> Result<bool, DeviceError> {
        *self.0.last.lock().expect("last period") = *frames;
        self.0.writes.fetch_add(1, Ordering::Relaxed);
        Ok(self.0.starting.swap(false, Ordering::Relaxed))
    }

    fn delay_frames(&mut self) -> Result<u32, DeviceError> {
        Ok(0)
    }

    fn recover_xrun(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }

    fn stop(&mut self) {
        self.0.stops.fetch_add(1, Ordering::Relaxed);
    }
}

fn mapped_ring(generation: u64) -> (StreamMemory, ProducerRing) {
    let memfd = linux_uapi::shared_memory::MemFd::create("audio-service-test", RING_MAPPING_BYTES)
        .expect("memfd");
    let mapping = SharedMapping::map(memfd.as_fd(), RING_MAPPING_BYTES).expect("mapping");
    // SAFETY: Newly created mapping is exclusively initialized before views.
    unsafe { initialize_ring(mapping.as_non_null(), mapping.len(), generation) }
        .expect("initialize");
    let producer = unsafe { ProducerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
        .expect("producer");
    let consumer = unsafe { ConsumerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
        .expect("consumer");
    (StreamMemory::new(mapping, consumer), producer)
}

#[test]
fn device_stays_stopped_until_start_and_pauses_without_idle_wake() {
    let commands = Arc::new(SpscQueue::new());
    let events = Arc::new(SpscQueue::new());
    let failed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DeviceState {
        activations: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        last: Mutex::new([[0.0; CHANNELS]; PERIOD_FRAMES]),
        permits: Mutex::new(0),
        ready: Condvar::new(),
        starting: AtomicBool::new(false),
    });
    let (memory, mut producer) = mapped_ring(1);
    producer
        .write(1, &[[0.25, -0.25]; PERIOD_FRAMES * 2])
        .expect("fill");
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
    let (event_reader, event_writer) = UnixStream::pair().expect("event wake");
    event_reader.set_nonblocking(true).expect("nonblocking");
    let mixer = Mixer::new(
        MockDevice(state.clone()),
        commands.clone(),
        events.clone(),
        event_writer,
        failed,
        1.0,
    );
    let handle = thread::spawn(move || mixer.run());
    loop {
        if matches!(events.pop(), Some(MixerEvent::Added { .. })) {
            break;
        }
        thread::yield_now();
    }
    assert_eq!(state.activations.load(Ordering::Relaxed), 0);
    assert!(
        commands
            .push(MixerCommand::Start {
                stream_id: 1,
                generation: 1,
            })
            .is_ok()
    );
    *state.permits.lock().expect("permits") = 2;
    state.ready.notify_one();
    handle.thread().unpark();
    while state.writes.load(Ordering::Relaxed) < 2 {
        thread::yield_now();
    }
    assert!(
        commands
            .push(MixerCommand::Pause {
                stream_id: 1,
                generation: 1,
            })
            .is_ok()
    );
    *state.permits.lock().expect("permits") = 1;
    state.ready.notify_one();
    while state.stops.load(Ordering::Relaxed) == 0 {
        thread::yield_now();
    }
    assert!(
        commands
            .push(MixerCommand::Start {
                stream_id: 1,
                generation: 1,
            })
            .is_ok()
    );
    *state.permits.lock().expect("permits") = 2;
    state.ready.notify_one();
    handle.thread().unpark();
    while state.writes.load(Ordering::Relaxed) < 4 {
        thread::yield_now();
    }
    assert!(commands.push(MixerCommand::Shutdown).is_ok());
    *state.permits.lock().expect("permits") = 1;
    state.ready.notify_one();
    handle.thread().unpark();
    handle.join().expect("join");
    drop(event_reader);
    assert_eq!(state.activations.load(Ordering::Relaxed), 2);
    assert!(state.stops.load(Ordering::Relaxed) >= 2);
}

#[test]
fn steady_mixer_metrics_measure_zero_allocations() {
    let commands = Arc::new(SpscQueue::new());
    let events = Arc::new(SpscQueue::new());
    let failed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DeviceState {
        activations: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        last: Mutex::new([[0.0; CHANNELS]; PERIOD_FRAMES]),
        permits: Mutex::new(188),
        ready: Condvar::new(),
        starting: AtomicBool::new(false),
    });
    for stream_id in 1..=MAX_SESSION_STREAMS as u64 {
        let (memory, mut producer) = mapped_ring(1);
        producer
            .write(
                1,
                &[[
                    0.125 / MAX_SESSION_STREAMS as f32,
                    -0.125 / MAX_SESSION_STREAMS as f32,
                ]; RING_CAPACITY_FRAMES],
            )
            .expect("fill");
        assert!(
            commands
                .push(MixerCommand::Add {
                    stream_id,
                    generation: 1,
                    gain: 1.0,
                    memory,
                })
                .is_ok()
        );
        assert!(
            commands
                .push(MixerCommand::Start {
                    stream_id,
                    generation: 1,
                })
                .is_ok()
        );
    }
    let (event_reader, event_writer) = UnixStream::pair().expect("event wake");
    let mixer = Mixer::new(
        MockDevice(state.clone()),
        commands.clone(),
        events.clone(),
        event_writer,
        failed,
        1.0,
    );
    let handle = thread::spawn(move || mixer.run());
    while state.writes.load(Ordering::Relaxed) < 188 {
        thread::yield_now();
    }
    assert!(commands.push(MixerCommand::Shutdown).is_ok());
    *state.permits.lock().expect("permits") = 1;
    state.ready.notify_one();
    handle.thread().unpark();
    handle.join().expect("join");
    drop(event_reader);

    let metrics = std::iter::from_fn(|| events.pop()).find_map(|event| match event {
        MixerEvent::Metrics(metrics) => Some(metrics),
        _ => None,
    });
    let metrics = metrics.expect("steady metrics event");
    assert_eq!(metrics.xrun_count, 0);
    assert_eq!(metrics.steady_allocations, 0);
    assert!(metrics.mix_p99_us <= 2_670);
}

#[test]
fn short_non_full_ring_reports_confirmed_drain() {
    let commands = Arc::new(SpscQueue::new());
    let events = Arc::new(SpscQueue::new());
    let failed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DeviceState {
        activations: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        last: Mutex::new([[0.0; CHANNELS]; PERIOD_FRAMES]),
        permits: Mutex::new(1),
        ready: Condvar::new(),
        starting: AtomicBool::new(false),
    });
    let (memory, mut producer) = mapped_ring(4);
    producer
        .write(4, &[[0.25, -0.25]; 100])
        .expect("short source");
    assert!(
        commands
            .push(MixerCommand::Add {
                stream_id: 7,
                generation: 4,
                gain: 1.0,
                memory,
            })
            .is_ok()
    );
    assert!(
        commands
            .push(MixerCommand::Start {
                stream_id: 7,
                generation: 4,
            })
            .is_ok()
    );
    let (event_reader, event_writer) = UnixStream::pair().expect("event wake");
    let mixer = Mixer::new(
        MockDevice(state.clone()),
        commands.clone(),
        events.clone(),
        event_writer,
        failed,
        1.0,
    );
    let handle = thread::spawn(move || mixer.run());
    while state.writes.load(Ordering::Relaxed) < 1 {
        thread::yield_now();
    }
    assert!(commands.push(MixerCommand::Shutdown).is_ok());
    *state.permits.lock().expect("permits") = 1;
    state.ready.notify_one();
    handle.thread().unpark();
    handle.join().expect("join");
    drop(event_reader);

    assert!(std::iter::from_fn(|| events.pop()).any(|event| matches!(
        event,
        MixerEvent::Progress {
            stream_id: 7,
            generation: 4,
            consumed_frames: 100
        }
    )));
}
