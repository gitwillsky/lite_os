use std::{
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use audio_proto::{CHANNELS, ConsumerRing, MAX_SESSION_STREAMS, RingError};
use linux_uapi::shared_memory::SharedMapping;

use crate::{
    limiter::{LOOKAHEAD_FRAMES, Limiter},
    queue::SpscQueue,
};

mod device;
mod metrics;
mod progress;
pub(crate) use device::DEVICE_PERIODS;
pub(crate) use device::{DEVICE_BUFFER_FRAMES, DeviceError, PlaybackDevice};
use metrics::{MixerMetrics, TimingHistogram};
use progress::PeriodHistory;

pub(crate) const PERIOD_FRAMES: usize = 256;
pub(crate) const COMMAND_CAPACITY: usize = 128;
pub(crate) const EVENT_CAPACITY: usize = 256;
const METRIC_PERIODS: u64 = 188;
const HISTORY_PERIODS: usize = 8;
const PROGRESS_EVENT_FRAMES: u64 = 4096;

/// Mapping lifetime paired with the ring's unique mixer-consumer view.
pub(crate) struct StreamMemory {
    mapping: SharedMapping,
    ring: ConsumerRing,
}

impl StreamMemory {
    pub(crate) fn new(mapping: SharedMapping, ring: ConsumerRing) -> Self {
        Self { mapping, ring }
    }

    fn ring(&mut self) -> &mut ConsumerRing {
        &mut self.ring
    }

    pub(crate) fn mapping_len(&self) -> usize {
        self.mapping.len()
    }
}

pub(crate) enum MixerCommand {
    Add {
        stream_id: u64,
        generation: u64,
        gain: f32,
        memory: StreamMemory,
    },
    Start {
        stream_id: u64,
        generation: u64,
    },
    Pause {
        stream_id: u64,
        generation: u64,
    },
    Gain {
        stream_id: u64,
        generation: u64,
        gain: f32,
    },
    Flush {
        stream_id: u64,
        old_generation: u64,
        new_generation: u64,
    },
    Close {
        stream_id: u64,
        generation: u64,
    },
    MasterGain(f32),
    Shutdown,
}

pub(crate) enum MixerEvent {
    Added {
        stream_id: u64,
        generation: u64,
    },
    Started {
        stream_id: u64,
        generation: u64,
    },
    Paused {
        stream_id: u64,
        generation: u64,
        consumed_frames: u64,
    },
    GainInstalled {
        stream_id: u64,
        generation: u64,
    },
    Flushed {
        stream_id: u64,
        generation: u64,
    },
    Closed {
        stream_id: u64,
        generation: u64,
        consumed_frames: u64,
        memory: StreamMemory,
    },
    Progress {
        stream_id: u64,
        generation: u64,
        consumed_frames: u64,
    },
    RingAvailable {
        stream_id: u64,
        generation: u64,
    },
    StreamError {
        stream_id: u64,
        generation: u64,
        error: RingError,
    },
    Metrics(MixerMetrics),
    DeviceStarted,
    DeviceStopped,
    DeviceFailed,
    Stopped,
}

struct Stream {
    id: u64,
    generation: u64,
    gain: f32,
    playing: bool,
    memory: StreamMemory,
    confirmed_frames: u64,
    reported_frames: u64,
    mixed_frames: u64,
    empty_confirmation_target: Option<u64>,
}

pub(crate) struct Mixer<D: PlaybackDevice> {
    device: D,
    commands: Arc<SpscQueue<MixerCommand, COMMAND_CAPACITY>>,
    events: Arc<SpscQueue<MixerEvent, EVENT_CAPACITY>>,
    event_wake: UnixStream,
    failed: Arc<AtomicBool>,
    streams: [Option<Stream>; MAX_SESSION_STREAMS],
    limiter: Limiter,
    master_gain: f32,
    // Records physical activation ownership. Without it idle iterations would
    // repeatedly START/DROP ALSA and create the forbidden periodic wake path.
    active_device: bool,
    submitted_frames: u64,
    history: [PeriodHistory; HISTORY_PERIODS],
    history_head: usize,
    history_tail: usize,
    delayed_counts: [u16; MAX_SESSION_STREAMS],
    period_count: u64,
    xrun_count: u64,
    timing: TimingHistogram,
}

impl<D: PlaybackDevice> Mixer<D> {
    pub(crate) fn new(
        device: D,
        commands: Arc<SpscQueue<MixerCommand, COMMAND_CAPACITY>>,
        events: Arc<SpscQueue<MixerEvent, EVENT_CAPACITY>>,
        event_wake: UnixStream,
        failed: Arc<AtomicBool>,
        master_gain: f32,
    ) -> Self {
        Self {
            device,
            commands,
            events,
            event_wake,
            failed,
            streams: std::array::from_fn(|_| None),
            limiter: Limiter::new(),
            master_gain,
            active_device: false,
            submitted_frames: 0,
            history: [PeriodHistory::EMPTY; HISTORY_PERIODS],
            history_head: 0,
            history_tail: 0,
            delayed_counts: [0; MAX_SESSION_STREAMS],
            period_count: 0,
            xrun_count: 0,
            timing: TimingHistogram::new(),
        }
    }

    pub(crate) fn run(mut self) {
        crate::allocation::begin_mixer_tracking();
        loop {
            if self.drain_commands() {
                self.stop_device();
                self.publish(MixerEvent::Stopped);
                return;
            }
            if !self.streams.iter().flatten().any(|stream| stream.playing) {
                self.stop_device();
                std::thread::park();
                continue;
            }
            if !self.active_device {
                if self.device.activate().is_err() {
                    self.fail_device();
                    return;
                }
                self.active_device = true;
            }
            match self.device.wait_period() {
                Ok(()) => {}
                Err(DeviceError::Xrun) => {
                    self.xrun_count = self.xrun_count.saturating_add(1);
                    if self.device.recover_xrun().is_err() {
                        self.fail_device();
                        return;
                    }
                    self.reset_pipeline();
                    continue;
                }
                Err(DeviceError::Fatal) => {
                    self.fail_device();
                    return;
                }
            }
            let delay = match self.device.delay_frames() {
                Ok(delay) => u64::from(delay),
                Err(_) => {
                    self.fail_device();
                    return;
                }
            };
            self.confirm_history(self.submitted_frames.saturating_sub(delay));
            if self.drain_commands() {
                self.stop_device();
                self.publish(MixerEvent::Stopped);
                return;
            }
            if !self.streams.iter().flatten().any(|stream| stream.playing) {
                continue;
            }
            let started = Instant::now();
            let mut mixed = [[0.0; CHANNELS]; PERIOD_FRAMES];
            let mut current_counts = [0u16; MAX_SESSION_STREAMS];
            for (slot, current_count) in current_counts.iter_mut().enumerate() {
                let event = {
                    let Some(stream) = self.streams[slot].as_mut().filter(|stream| stream.playing)
                    else {
                        continue;
                    };
                    let mixed_result = {
                        let ring = stream.memory.ring();
                        ring.mix_into(stream.generation, stream.gain, &mut mixed)
                            .and_then(|(count, became_available)| {
                                let snapshot = ring.snapshot()?;
                                Ok((
                                    count,
                                    became_available,
                                    snapshot.produced_frames == snapshot.consumed_frames,
                                ))
                            })
                    };
                    match mixed_result {
                        Ok((count, became_available, became_empty)) => {
                            *current_count = count as u16;
                            if count != 0 {
                                stream.mixed_frames =
                                    stream.mixed_frames.saturating_add(count as u64);
                                stream.empty_confirmation_target =
                                    became_empty.then_some(stream.mixed_frames);
                            }
                            became_available.then_some(MixerEvent::RingAvailable {
                                stream_id: stream.id,
                                generation: stream.generation,
                            })
                        }
                        Err(error) => {
                            stream.playing = false;
                            Some(MixerEvent::StreamError {
                                stream_id: stream.id,
                                generation: stream.generation,
                                error,
                            })
                        }
                    }
                };
                if let Some(event) = event {
                    self.publish(event);
                }
            }
            for frame in &mut mixed {
                for sample in frame {
                    *sample *= self.master_gain;
                }
            }
            let mut output = [[0.0; CHANNELS]; PERIOD_FRAMES];
            for block in 0..2 {
                let start = block * LOOKAHEAD_FRAMES;
                let input: &[[f32; CHANNELS]; LOOKAHEAD_FRAMES] = mixed
                    [start..start + LOOKAHEAD_FRAMES]
                    .try_into()
                    .expect("fixed limiter input block");
                let destination: &mut [[f32; CHANNELS]; LOOKAHEAD_FRAMES] = (&mut output
                    [start..start + LOOKAHEAD_FRAMES])
                    .try_into()
                    .expect("fixed limiter output block");
                self.limiter.process(input, destination);
            }

            match self.device.write_period(&output) {
                Ok(started) => {
                    if started {
                        self.publish(MixerEvent::DeviceStarted);
                        crate::allocation::reset_mixer_tracking();
                    }
                }
                Err(error) => {
                    if error == DeviceError::Xrun {
                        self.xrun_count = self.xrun_count.saturating_add(1);
                        if self.device.recover_xrun().is_ok() {
                            self.reset_pipeline();
                            continue;
                        }
                    }
                    self.fail_device();
                    return;
                }
            }
            // One 128-frame lookahead means this period submits the previous
            // period's second half plus current first half attribution. Reporting
            // a conservative full-period count delays progress by at most one
            // period while never claiming unsubmitted source frames.
            let mut submitted_counts = self.delayed_counts;
            for slot in 0..MAX_SESSION_STREAMS {
                submitted_counts[slot] = submitted_counts[slot]
                    .saturating_add(current_counts[slot].min(LOOKAHEAD_FRAMES as u16));
                self.delayed_counts[slot] =
                    current_counts[slot].saturating_sub(LOOKAHEAD_FRAMES as u16);
            }
            self.submitted_frames = self.submitted_frames.saturating_add(PERIOD_FRAMES as u64);
            self.push_history(PeriodHistory {
                submitted_end: self.submitted_frames,
                counts: submitted_counts,
            });
            let delay = match self.device.delay_frames() {
                Ok(delay) => u64::from(delay),
                Err(_) => {
                    self.fail_device();
                    return;
                }
            };
            self.confirm_history(self.submitted_frames.saturating_sub(delay));
            self.period_count = self.period_count.saturating_add(1);
            self.timing.record(started.elapsed().as_micros() as u64);
            if self.period_count.is_multiple_of(METRIC_PERIODS) {
                self.publish(MixerEvent::Metrics(self.metrics()));
            }
        }
    }

    fn drain_commands(&mut self) -> bool {
        while let Some(command) = self.commands.pop() {
            match command {
                MixerCommand::Add {
                    stream_id,
                    generation,
                    gain,
                    memory,
                } => {
                    if let Some(slot) = self.streams.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(Stream {
                            id: stream_id,
                            generation,
                            gain,
                            playing: false,
                            memory,
                            confirmed_frames: 0,
                            reported_frames: 0,
                            mixed_frames: 0,
                            empty_confirmation_target: None,
                        });
                        self.publish(MixerEvent::Added {
                            stream_id,
                            generation,
                        });
                    } else {
                        self.publish(MixerEvent::StreamError {
                            stream_id,
                            generation,
                            error: RingError::CorruptIndices,
                        });
                    }
                }
                MixerCommand::Start {
                    stream_id,
                    generation,
                } => {
                    if let Some(stream) = self.stream(stream_id, generation) {
                        stream.playing = true;
                        self.publish(MixerEvent::Started {
                            stream_id,
                            generation,
                        });
                    }
                }
                MixerCommand::Pause {
                    stream_id,
                    generation,
                } => {
                    if let Some(stream) = self.stream(stream_id, generation) {
                        stream.playing = false;
                        let event = MixerEvent::Paused {
                            stream_id,
                            generation,
                            consumed_frames: stream.confirmed_frames,
                        };
                        self.publish(event);
                    }
                }
                MixerCommand::Gain {
                    stream_id,
                    generation,
                    gain,
                } => {
                    if let Some(stream) = self.stream(stream_id, generation) {
                        stream.gain = gain;
                        self.publish(MixerEvent::GainInstalled {
                            stream_id,
                            generation,
                        });
                    }
                }
                MixerCommand::Flush {
                    stream_id,
                    old_generation,
                    new_generation,
                } => {
                    let event = if let Some(stream) = self.stream(stream_id, old_generation) {
                        stream.playing = false;
                        match stream.memory.ring().flush(old_generation, new_generation) {
                            Ok(()) => {
                                stream.generation = new_generation;
                                stream.confirmed_frames = 0;
                                stream.reported_frames = 0;
                                stream.mixed_frames = 0;
                                stream.empty_confirmation_target = None;
                                MixerEvent::Flushed {
                                    stream_id,
                                    generation: new_generation,
                                }
                            }
                            Err(error) => MixerEvent::StreamError {
                                stream_id,
                                generation: old_generation,
                                error,
                            },
                        }
                    } else {
                        continue;
                    };
                    self.publish(event);
                }
                MixerCommand::Close {
                    stream_id,
                    generation,
                } => {
                    if let Some(slot) = self.stream_slot(stream_id, generation) {
                        let stream = self.streams[slot].take().expect("matched stream");
                        self.publish(MixerEvent::Closed {
                            stream_id,
                            generation,
                            consumed_frames: stream.confirmed_frames,
                            memory: stream.memory,
                        });
                    }
                }
                MixerCommand::MasterGain(gain) => self.master_gain = gain,
                MixerCommand::Shutdown => return true,
            }
        }
        false
    }

    fn stream(&mut self, id: u64, generation: u64) -> Option<&mut Stream> {
        self.streams
            .iter_mut()
            .flatten()
            .find(|stream| stream.id == id && stream.generation == generation)
    }

    fn stream_slot(&self, id: u64, generation: u64) -> Option<usize> {
        self.streams.iter().position(|stream| {
            stream
                .as_ref()
                .is_some_and(|stream| stream.id == id && stream.generation == generation)
        })
    }

    fn metrics(&self) -> MixerMetrics {
        MixerMetrics {
            xrun_count: self.xrun_count,
            period_count: self.period_count,
            mix_p99_us: self.timing.percentile_99_us(),
            limiter_activations: self.limiter.activations(),
            limiter_max_reduction: self.limiter.maximum_reduction(),
            steady_allocations: crate::allocation::mixer_allocations(),
        }
    }

    fn publish(&self, event: MixerEvent) {
        match self.events.push(event) {
            Ok(true) => {
                // This one-byte edge is readiness only, never control framing.
                // The event queue is authoritative; nonblocking failure is fatal
                // because control could otherwise sleep with queued cleanup.
                let mut wake = &self.event_wake;
                if wake.write(&[1]).is_err() {
                    self.failed.store(true, Ordering::Release);
                }
            }
            Ok(false) => {}
            Err(_) => self.failed.store(true, Ordering::Release),
        }
    }

    fn fail_device(&mut self) {
        self.stop_device();
        self.publish(MixerEvent::DeviceFailed);
    }

    fn stop_device(&mut self) {
        if self.active_device {
            self.device.stop();
            self.active_device = false;
            self.publish(MixerEvent::DeviceStopped);
            self.reset_pipeline();
        }
    }

    fn reset_pipeline(&mut self) {
        self.submitted_frames = 0;
        self.history_head = 0;
        self.history_tail = 0;
        self.delayed_counts.fill(0);
        self.limiter = Limiter::new();
    }
}

#[cfg(test)]
mod tests;
