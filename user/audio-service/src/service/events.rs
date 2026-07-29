use std::io;

use audio_proto::{AckOperation, ErrorCode, ServiceMessage};

use super::{Service, StreamPhase};
use crate::mixer::{DEVICE_BUFFER_FRAMES, MixerCommand, MixerEvent, PERIOD_FRAMES, PlaybackDevice};

impl<D: PlaybackDevice> Service<D> {
    pub(super) fn drain_mixer_events(&mut self) -> io::Result<bool> {
        while let Some(event) = self.events.pop() {
            match event {
                MixerEvent::Added {
                    stream_id,
                    generation,
                } => {
                    eprintln!(
                        "audio-service: stream create id={stream_id} generation={generation} ring_frames={}",
                        audio_proto::RING_CAPACITY_FRAMES
                    );
                }
                MixerEvent::Started {
                    stream_id,
                    generation,
                } => {
                    eprintln!("audio-service: stream start id={stream_id} generation={generation}");
                    if let Some(record) = self
                        .streams
                        .iter_mut()
                        .find(|record| record.id == stream_id && record.generation == generation)
                    {
                        record.playing = true;
                    }
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Ack {
                            stream_id,
                            generation,
                            operation: AckOperation::Start,
                        },
                    )?;
                }
                MixerEvent::Paused {
                    stream_id,
                    generation,
                    consumed_frames,
                } => {
                    if let Some(record) = self
                        .streams
                        .iter_mut()
                        .find(|record| record.id == stream_id && record.generation == generation)
                    {
                        record.confirmed_frames = consumed_frames;
                        record.playing = false;
                    }
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Progress {
                            stream_id,
                            generation,
                            consumed_frames,
                            concurrent_playbacks: self.concurrent_playbacks(),
                        },
                    )?;
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Ack {
                            stream_id,
                            generation,
                            operation: AckOperation::Pause,
                        },
                    )?;
                }
                MixerEvent::GainInstalled {
                    stream_id,
                    generation,
                } => self.route(
                    stream_id,
                    generation,
                    ServiceMessage::Ack {
                        stream_id,
                        generation,
                        operation: AckOperation::Gain,
                    },
                )?,
                MixerEvent::Flushed {
                    stream_id,
                    generation,
                } => {
                    if let Some(record) = self.streams.iter_mut().find(|record| {
                        record.id == stream_id && record.phase == StreamPhase::Flushing(generation)
                    }) {
                        record.generation = generation;
                        record.phase = StreamPhase::Live;
                        record.playing = false;
                        record.confirmed_frames = 0;
                        record.next_progress_marker = 1;
                    }
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Flushed {
                            stream_id,
                            generation,
                        },
                    )?;
                }
                MixerEvent::Closed {
                    stream_id,
                    generation,
                    consumed_frames,
                    memory,
                } => {
                    eprintln!(
                        "audio-service: stream close id={stream_id} generation={generation} consumed_frames={consumed_frames}"
                    );
                    let _ = memory.mapping_len();
                    let connection = self
                        .streams
                        .iter()
                        .find(|record| record.id == stream_id)
                        .map(|record| record.connection);
                    self.streams.retain(|record| record.id != stream_id);
                    if let Some(connection) = connection
                        && let Some(index) = self.connection_index(connection)
                    {
                        let _ = self.send(
                            index,
                            ServiceMessage::Ack {
                                stream_id,
                                generation,
                                operation: AckOperation::Close,
                            },
                        );
                    }
                    drop(memory);
                }
                MixerEvent::Progress {
                    stream_id,
                    generation,
                    consumed_frames,
                } => {
                    if let Some(record) = self
                        .streams
                        .iter_mut()
                        .find(|record| record.id == stream_id && record.generation == generation)
                    {
                        record.confirmed_frames = consumed_frames;
                        if self.diagnostic_log && consumed_frames >= record.next_progress_marker {
                            eprintln!(
                                "audio-service: stream progress id={stream_id} generation={generation} consumed_frames={consumed_frames}"
                            );
                            record.next_progress_marker =
                                (consumed_frames / 48_000 + 1).saturating_mul(48_000);
                        }
                    }
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Progress {
                            stream_id,
                            generation,
                            consumed_frames,
                            concurrent_playbacks: self.concurrent_playbacks(),
                        },
                    )?;
                }
                MixerEvent::RingAvailable {
                    stream_id,
                    generation,
                } => self.route(
                    stream_id,
                    generation,
                    ServiceMessage::RingAvailable {
                        stream_id,
                        generation,
                    },
                )?,
                MixerEvent::StreamError {
                    stream_id,
                    generation,
                    error,
                } => {
                    let _ = error;
                    self.route(
                        stream_id,
                        generation,
                        ServiceMessage::Error {
                            stream_id: Some(stream_id),
                            generation,
                            code: ErrorCode::CorruptRing,
                        },
                    )?;
                    if let Some(record) = self
                        .streams
                        .iter_mut()
                        .find(|record| record.id == stream_id)
                    {
                        record.phase = StreamPhase::Closing;
                        record.playing = false;
                    }
                    self.push_command(MixerCommand::Close {
                        stream_id,
                        generation,
                    })?;
                }
                MixerEvent::Metrics(metrics) => {
                    if self.diagnostic_log {
                        eprintln!(
                            "audio-service: metrics periods={} xrun={} steady_allocations={} idle_periodic_wakes=0 mix_p99_us={} limiter_activations={} limiter_max_reduction={:.6}",
                            metrics.period_count,
                            metrics.xrun_count,
                            metrics.steady_allocations,
                            metrics.mix_p99_us,
                            metrics.limiter_activations,
                            metrics.limiter_max_reduction,
                        );
                    }
                }
                MixerEvent::DeviceStarted => eprintln!(
                    "audio-service: device start rate_hz={} channels={} period_frames={} buffer_frames={}",
                    audio_proto::SAMPLE_RATE,
                    audio_proto::CHANNELS,
                    PERIOD_FRAMES,
                    DEVICE_BUFFER_FRAMES
                ),
                MixerEvent::DeviceStopped => eprintln!("audio-service: device stop"),
                MixerEvent::DeviceFailed => {
                    for index in 0..self.connections.len() {
                        let _ = self.send_error(index, None, 0, ErrorCode::DeviceFailure);
                    }
                    return Ok(true);
                }
                MixerEvent::Stopped => return Ok(false),
            }
        }
        Ok(false)
    }

    fn route(
        &mut self,
        stream_id: u64,
        generation: u64,
        message: ServiceMessage,
    ) -> io::Result<()> {
        let connection = self
            .streams
            .iter()
            .find(|record| {
                record.id == stream_id
                    && (record.generation == generation
                        || record.phase == StreamPhase::Flushing(generation))
            })
            .map(|record| record.connection);
        if let Some(connection) = connection
            && let Some(index) = self.connection_index(connection)
            && !self.send(index, message)?
        {
            let identity = self.connections[index].identity;
            self.connections.swap_remove(index);
            self.close_connection_streams(identity)?;
        }
        Ok(())
    }
}
