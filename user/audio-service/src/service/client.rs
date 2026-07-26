use std::{io, os::fd::AsFd, time::Instant};

use audio_proto::{
    ClientMessage, ClientRole, ConsumerRing, ErrorCode, MAX_SESSION_STREAMS,
    MAX_STREAMS_PER_CONNECTION, PROTOCOL_VERSION, RING_CAPACITY_FRAMES, RING_MAPPING_BYTES,
    ServiceMessage, initialize_ring, send_service,
};
use linux_uapi::shared_memory::{MemFd, SharedMapping};

use super::{Service, StreamPhase, StreamRecord};
use crate::mixer::{MixerCommand, PlaybackDevice, StreamMemory};

impl<D: PlaybackDevice> Service<D> {
    pub(super) fn handle_message(
        &mut self,
        index: usize,
        message: ClientMessage,
    ) -> io::Result<bool> {
        let connection_id = self.connections[index].identity;
        let role = self.connections[index].role;
        if role.is_none() {
            return match message {
                ClientMessage::Hello { version, role } if version == PROTOCOL_VERSION => {
                    self.connections[index].role = Some(role);
                    self.send(
                        index,
                        ServiceMessage::Welcome {
                            version: PROTOCOL_VERSION,
                        },
                    )
                }
                ClientMessage::Hello { .. } => {
                    let _ = self.send_error(index, None, 0, ErrorCode::ProtocolMismatch);
                    Ok(false)
                }
                _ => Ok(false),
            };
        }
        match (role.expect("handshake role"), message) {
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::CreateStream { generation },
            ) => self.create_stream(index, generation),
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::Start {
                    stream_id,
                    generation,
                },
            ) => self.stream_command(
                index,
                stream_id,
                generation,
                MixerCommand::Start {
                    stream_id,
                    generation,
                },
            ),
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::Pause {
                    stream_id,
                    generation,
                },
            ) => self.stream_command(
                index,
                stream_id,
                generation,
                MixerCommand::Pause {
                    stream_id,
                    generation,
                },
            ),
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::SetGain {
                    stream_id,
                    generation,
                    gain,
                },
            ) => {
                if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
                    return self.send_error(
                        index,
                        Some(stream_id),
                        generation,
                        ErrorCode::InvalidState,
                    );
                }
                self.stream_command(
                    index,
                    stream_id,
                    generation,
                    MixerCommand::Gain {
                        stream_id,
                        generation,
                        gain,
                    },
                )
            }
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::Flush {
                    stream_id,
                    old_generation,
                    new_generation,
                },
            ) => self.flush_stream(index, stream_id, old_generation, new_generation),
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::Close {
                    stream_id,
                    generation,
                },
            ) => self.close_stream(index, stream_id, generation),
            (
                ClientRole::Media | ClientRole::Desktop,
                ClientMessage::RingNonempty {
                    stream_id,
                    generation,
                },
            ) => {
                if self.record(connection_id, stream_id, generation).is_none() {
                    self.send_error(
                        index,
                        Some(stream_id),
                        generation,
                        self.stream_lookup_error(connection_id, stream_id),
                    )
                } else {
                    if let Some(handle) = &self.mixer {
                        handle.thread().unpark();
                    }
                    Ok(true)
                }
            }
            (ClientRole::Desktop, ClientMessage::GetMasterState) => self.send_master(index),
            (ClientRole::Desktop, ClientMessage::SetMasterVolume { percent }) => {
                if percent > 100 {
                    return self.send_error(index, None, 0, ErrorCode::InvalidState);
                }
                if percent != self.settings.state().percent {
                    let mut proposed = self.settings.state();
                    proposed.percent = percent;
                    if !self.push_for_client(
                        index,
                        None,
                        0,
                        MixerCommand::MasterGain(proposed.gain()),
                    )? {
                        return Ok(true);
                    }
                    if self.settings.set_percent(percent, Instant::now()) {
                        log_master_change(
                            self.settings.state().percent,
                            self.settings.state().muted,
                        );
                    }
                }
                self.broadcast_master()?;
                Ok(true)
            }
            (ClientRole::Desktop, ClientMessage::SetMasterMuted { muted }) => {
                if muted != self.settings.state().muted {
                    let mut proposed = self.settings.state();
                    proposed.muted = muted;
                    if !self.push_for_client(
                        index,
                        None,
                        0,
                        MixerCommand::MasterGain(proposed.gain()),
                    )? {
                        return Ok(true);
                    }
                    if self.settings.set_muted(muted, Instant::now()) {
                        log_master_change(
                            self.settings.state().percent,
                            self.settings.state().muted,
                        );
                    }
                }
                self.broadcast_master()?;
                Ok(true)
            }
            _ => self.send_error(index, None, 0, ErrorCode::PermissionDenied),
        }
    }

    fn create_stream(&mut self, index: usize, generation: u64) -> io::Result<bool> {
        let connection = self.connections[index].identity;
        let pid = self.connections[index].pid;
        if generation == 0 || quota_exhausted(&self.streams, pid) {
            return self.send_error(index, None, generation, ErrorCode::QuotaExceeded);
        }
        let stream_id = self.next_stream;
        self.next_stream = self
            .next_stream
            .checked_add(1)
            .ok_or_else(|| io::Error::other("audio stream identity exhausted"))?;
        let memfd = MemFd::create("liteos-audio-ring", RING_MAPPING_BYTES)?;
        let mapping = SharedMapping::map(memfd.as_fd(), RING_MAPPING_BYTES)?;
        // SAFETY: The new mapping is exclusive until queue/SCM_RIGHTS publication.
        unsafe { initialize_ring(mapping.as_non_null(), mapping.len(), generation) }
            .map_err(|_| io::Error::other("audio ring initialization failed"))?;
        let consumer = unsafe { ConsumerRing::from_mapping(mapping.as_non_null(), mapping.len()) }
            .map_err(|_| io::Error::other("audio ring consumer projection failed"))?;
        if !self.push_for_client(
            index,
            None,
            generation,
            MixerCommand::Add {
                stream_id,
                generation,
                gain: 1.0,
                memory: StreamMemory::new(mapping, consumer),
            },
        )? {
            return Ok(true);
        }
        self.streams.push(StreamRecord {
            connection,
            pid,
            id: stream_id,
            generation,
            phase: StreamPhase::Live,
            confirmed_frames: 0,
            next_progress_marker: 1,
        });
        let sent = send_service(
            &self.connections[index].stream,
            ServiceMessage::StreamCreated {
                stream_id,
                generation,
                capacity_frames: RING_CAPACITY_FRAMES as u32,
            },
            Some(memfd.as_fd()),
        );
        if sent.is_err() {
            if let Some(record) = self
                .streams
                .iter_mut()
                .find(|record| record.connection == connection && record.id == stream_id)
            {
                record.phase = StreamPhase::Closing;
            }
            self.push_command(MixerCommand::Close {
                stream_id,
                generation,
            })?;
            return Ok(false);
        }
        Ok(true)
    }

    fn stream_command(
        &mut self,
        index: usize,
        stream_id: u64,
        generation: u64,
        command: MixerCommand,
    ) -> io::Result<bool> {
        let connection = self.connections[index].identity;
        match self.record(connection, stream_id, generation) {
            Some(record) if record.phase == StreamPhase::Live => {
                self.push_for_client(index, Some(stream_id), generation, command)?;
                Ok(true)
            }
            Some(_) => self.send_error(index, Some(stream_id), generation, ErrorCode::InvalidState),
            None => self.send_error(
                index,
                Some(stream_id),
                generation,
                self.stream_lookup_error(connection, stream_id),
            ),
        }
    }

    fn flush_stream(
        &mut self,
        index: usize,
        stream_id: u64,
        old_generation: u64,
        new_generation: u64,
    ) -> io::Result<bool> {
        let connection = self.connections[index].identity;
        let Some(record) = self.record_mut(connection, stream_id, old_generation) else {
            return self.send_error(
                index,
                Some(stream_id),
                old_generation,
                self.stream_lookup_error(connection, stream_id),
            );
        };
        if record.phase != StreamPhase::Live || new_generation <= old_generation {
            return self.send_error(
                index,
                Some(stream_id),
                old_generation,
                ErrorCode::InvalidState,
            );
        }
        if !self.push_for_client(
            index,
            Some(stream_id),
            old_generation,
            MixerCommand::Flush {
                stream_id,
                old_generation,
                new_generation,
            },
        )? {
            return Ok(true);
        }
        self.record_mut(connection, stream_id, old_generation)
            .expect("validated flush record")
            .phase = StreamPhase::Flushing(new_generation);
        Ok(true)
    }

    fn close_stream(&mut self, index: usize, stream_id: u64, generation: u64) -> io::Result<bool> {
        let connection = self.connections[index].identity;
        let Some(record) = self.record_mut(connection, stream_id, generation) else {
            return self.send_error(
                index,
                Some(stream_id),
                generation,
                self.stream_lookup_error(connection, stream_id),
            );
        };
        if record.phase != StreamPhase::Live {
            return self.send_error(index, Some(stream_id), generation, ErrorCode::InvalidState);
        }
        if !self.push_for_client(
            index,
            Some(stream_id),
            generation,
            MixerCommand::Close {
                stream_id,
                generation,
            },
        )? {
            return Ok(true);
        }
        self.record_mut(connection, stream_id, generation)
            .expect("validated close record")
            .phase = StreamPhase::Closing;
        Ok(true)
    }

    fn push_for_client(
        &self,
        index: usize,
        stream_id: Option<u64>,
        generation: u64,
        command: MixerCommand,
    ) -> io::Result<bool> {
        match self.push_command(command) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.send_error(index, stream_id, generation, ErrorCode::Backpressure)?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn stream_lookup_error(&self, connection: u64, stream_id: u64) -> ErrorCode {
        if self
            .streams
            .iter()
            .any(|stream| stream.connection == connection && stream.id == stream_id)
        {
            ErrorCode::StaleGeneration
        } else {
            ErrorCode::UnknownStream
        }
    }

    pub(super) fn close_connection_streams(&mut self, connection: u64) -> io::Result<()> {
        let streams = self
            .streams
            .iter_mut()
            .filter(|stream| stream.connection == connection)
            .filter_map(|stream| {
                if stream.phase == StreamPhase::Closing {
                    None
                } else {
                    let generation = stream.mixer_generation();
                    stream.phase = StreamPhase::Closing;
                    Some((stream.id, generation))
                }
            })
            .collect::<Vec<_>>();
        for (stream_id, generation) in streams {
            self.push_command(MixerCommand::Close {
                stream_id,
                generation,
            })?;
        }
        Ok(())
    }

    fn broadcast_master(&mut self) -> io::Result<()> {
        let state = self.settings.state();
        let recipients = self
            .connections
            .iter()
            .enumerate()
            .filter_map(|(index, connection)| {
                (connection.role == Some(ClientRole::Desktop)).then_some(index)
            })
            .collect::<Vec<_>>();
        for index in recipients {
            let _ = self.send(
                index,
                ServiceMessage::MasterState {
                    percent: state.percent,
                    muted: state.muted,
                },
            )?;
        }
        Ok(())
    }

    fn send_master(&self, index: usize) -> io::Result<bool> {
        let state = self.settings.state();
        self.send(
            index,
            ServiceMessage::MasterState {
                percent: state.percent,
                muted: state.muted,
            },
        )
    }
}

fn quota_exhausted(streams: &[StreamRecord], pid: i32) -> bool {
    streams.len() >= MAX_SESSION_STREAMS
        || streams.iter().filter(|stream| stream.pid == pid).count() >= MAX_STREAMS_PER_CONNECTION
}

fn master_marker(percent: u8, muted: bool) -> String {
    format!("audio-service: master percent={percent} muted={muted}")
}

fn log_master_change(percent: u8, muted: bool) {
    eprintln!("{}", master_marker(percent, muted));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(connection: u64, id: u64) -> StreamRecord {
        StreamRecord {
            connection,
            pid: connection as i32,
            id,
            generation: 1,
            phase: StreamPhase::Live,
            confirmed_frames: 0,
            next_progress_marker: 1,
        }
    }

    #[test]
    fn app_and_session_quotas_are_atomic_boundaries() {
        let mut streams = (1..=MAX_STREAMS_PER_CONNECTION as u64)
            .map(|id| record(7, id))
            .collect::<Vec<_>>();
        assert!(quota_exhausted(&streams, 7));
        assert!(!quota_exhausted(&streams, 8));

        streams.clear();
        for id in 1..=MAX_SESSION_STREAMS as u64 {
            streams.push(record(id / MAX_STREAMS_PER_CONNECTION as u64 + 1, id));
        }
        assert!(quota_exhausted(&streams, 99));
    }

    #[test]
    fn disconnect_closes_the_generation_that_flush_will_publish() {
        let record = StreamRecord {
            phase: StreamPhase::Flushing(12),
            ..record(1, 2)
        };
        assert_eq!(record.mixer_generation(), 12);
    }

    #[test]
    fn master_change_marker_carries_the_authoritative_state() {
        assert_eq!(
            master_marker(70, false),
            "audio-service: master percent=70 muted=false"
        );
        assert_eq!(
            master_marker(75, true),
            "audio-service: master percent=75 muted=true"
        );
    }
}
