//! Per-process media worker and the sole client path to the system audio service.

mod control;
mod decode;
mod service;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    io::{Read, Write as _},
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{Arc, Mutex},
    time::Instant,
};

use audio_proto::{
    CHANNELS, ClientMessage, ClientRole, MAX_FRAME_LEN, MappedProducer, PROTOCOL_VERSION,
    RING_CAPACITY_FRAMES, SOCKET_PATH, ServiceMessage, recv_service, send_client,
};
use linux_uapi::unix::{self, PollEvents, PollFd};

use self::decode::DecoderSession;
pub(crate) use control::{Command, Commands, Event, Events, start};
// Public streaming infrastructure, re-exported at the crate root for app
// extensions that download-while-playing. `SharedStream` is used within the
// audio pipeline; `StreamState` is re-exported for extensions to construct.
pub use decode::{SharedStream, StreamState};

const RENDER_FRAMES: usize = 128;
const PREFILL_BLOCKS: usize = RING_CAPACITY_FRAMES / RENDER_FRAMES;

/// Emits one complete cross-process diagnostic record with a single write.
///
/// Formatting directly through `eprintln!` can split a record into several
/// writes; concurrent Music workers then splice their bytes and destroy the
/// event ordering barriers consumed by the runtime gate.
fn diagnostic(arguments: fmt::Arguments<'_>) {
    let mut record = String::new();
    if fmt::write(&mut record, arguments).is_err() {
        return;
    }
    record.push('\n');
    let _ = std::io::stderr().write_all(record.as_bytes());
}

enum FillResult {
    Written,
    Full,
    Eof,
}

struct Media {
    decoder: DecoderSession,
    generation: u64,
    stream_id: Option<u64>,
    ring: Option<MappedProducer>,
    playing: bool,
    starting: bool,
    ending: bool,
    loop_enabled: bool,
    gain: f32,
    base_seconds: f64,
    submitted_frames: u64,
    consumed_frames: u64,
    decode_eof: bool,
    last_timeupdate: Instant,
    resume_position: Option<f64>,
}

struct Worker {
    role: ClientRole,
    commands: Arc<Mutex<VecDeque<Command>>>,
    command_wake: UnixStream,
    events: Arc<Mutex<VecDeque<Event>>>,
    event_wake: UnixStream,
    media: BTreeMap<u64, Media>,
    // Owns Close commands awaiting their asynchronous acknowledgements. Without
    // this state a later create barrier cannot distinguish a legal retired-stream
    // frame from an unknown stream protocol violation.
    closing_streams: BTreeMap<u64, u64>,
    service: Option<UnixStream>,
    service_bytes: [u8; MAX_FRAME_LEN],
    deferred_service: VecDeque<ServiceMessage>,
    /// Logical CPUs available to this UA process. Combined with the service's
    /// exact concurrent playback count, this keeps aggregate periodic media UI
    /// work bounded while preserving 4 Hz for the ordinary one-stream case.
    render_parallelism: u32,
}

impl Worker {
    fn new(
        role: ClientRole,
        commands: Arc<Mutex<VecDeque<Command>>>,
        command_wake: UnixStream,
        events: Arc<Mutex<VecDeque<Event>>>,
        event_wake: UnixStream,
    ) -> Self {
        Self {
            role,
            commands,
            command_wake,
            events,
            event_wake,
            media: BTreeMap::new(),
            closing_streams: BTreeMap::new(),
            service: None,
            service_bytes: [0; MAX_FRAME_LEN],
            deferred_service: VecDeque::new(),
            render_parallelism: std::thread::available_parallelism()
                .map(|count| count.get().try_into().unwrap_or(u32::MAX))
                .unwrap_or(1),
        }
    }

    fn run(mut self) {
        loop {
            let service_fd = self.service.as_ref().map(AsFd::as_fd);
            let mut descriptors = Vec::with_capacity(2);
            descriptors.push(PollFd::new(self.command_wake.as_fd(), PollEvents::READ));
            if let Some(fd) = service_fd {
                descriptors.push(PollFd::new(fd, PollEvents::READ));
            }
            if let Err(error) = unix::poll(&mut descriptors, None) {
                self.fail_all(format!("audio worker poll failed: {error}"));
                return;
            }
            if descriptors[0].returned() != PollEvents::EMPTY {
                let mut wake = [0; 256];
                let _ = self.command_wake.read(&mut wake);
                if self.commands.is_poisoned() {
                    self.fail_all("audio command queue is poisoned".to_owned());
                    return;
                }
                let commands: Vec<_> = {
                    let mut queue = self
                        .commands
                        .lock()
                        .expect("poison was checked on the sole worker");
                    queue.drain(..).collect()
                };
                for command in commands {
                    if let Err((id, error)) = self.handle(command) {
                        if let Some(media) = self.media.get_mut(&id) {
                            media.starting = false;
                        }
                        self.emit(Event::error(id, 3, error));
                    }
                }
            }
            if descriptors
                .get(1)
                .is_some_and(|descriptor| descriptor.returned() != PollEvents::EMPTY)
                && let Err(error) = self.receive_service()
            {
                self.disconnect_service(error);
            }
        }
    }

    fn handle(&mut self, command: Command) -> Result<(), (u64, String)> {
        match command {
            Command::Load {
                id,
                path,
                offset,
                length,
                stream,
            } => {
                self.close_stream(id);
                self.media.remove(&id);
                let decoder = DecoderSession::open_source(&path, offset, length, stream)
                    .map_err(|error| (id, error))?;
                let duration = decoder.metadata().duration;
                diagnostic(format_args!(
                    "LITE_AUDIO source-opened id={id} file={}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("media")
                ));
                self.media.insert(
                    id,
                    Media {
                        decoder,
                        generation: 1,
                        stream_id: None,
                        ring: None,
                        playing: false,
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
                self.emit(Event {
                    duration: Some(duration),
                    ..Event::plain(id, "durationchange")
                });
                self.emit(Event::plain(id, "loadedmetadata"));
            }
            Command::Play { id } => {
                if let Some(position) = self
                    .media
                    .get(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?
                    .resume_position
                {
                    self.reset_timeline(id, position)
                        .map_err(|error| (id, error))?;
                }
                let media = self
                    .media
                    .get_mut(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?;
                if media.playing || media.starting {
                    return Ok(());
                }
                media.starting = true;
                self.emit(Event::plain(id, "play"));
                self.ensure_stream(id).map_err(|error| (id, error))?;
                self.prefill(id).map_err(|error| (id, error))?;
                let media = self
                    .media
                    .get(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?;
                let stream_id = media
                    .stream_id
                    .ok_or((id, "audio stream was not created".to_owned()))?;
                let generation = media.generation;
                self.send(ClientMessage::Start {
                    stream_id,
                    generation,
                })
                .map_err(|error| (id, error))?;
            }
            Command::Pause { id } => {
                let media = self
                    .media
                    .get(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?;
                if !media.playing && !media.starting {
                    return Ok(());
                }
                if let Some(stream_id) = media.stream_id {
                    self.send(ClientMessage::Pause {
                        stream_id,
                        generation: media.generation,
                    })
                    .map_err(|error| (id, error))?;
                } else {
                    self.emit(Event::time(id, "timeupdate", media.base_seconds));
                    self.emit(Event::plain(id, "pause"));
                }
            }
            Command::Seek { id, seconds } => self.seek(id, seconds).map_err(|error| (id, error))?,
            Command::Gain { id, gain } => {
                let media = self
                    .media
                    .get_mut(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?;
                media.gain = gain;
                if let Some(stream_id) = media.stream_id {
                    let generation = media.generation;
                    self.send(ClientMessage::SetGain {
                        stream_id,
                        generation,
                        gain,
                    })
                    .map_err(|error| (id, error))?;
                }
            }
            Command::Loop { id, enabled } => {
                self.media
                    .get_mut(&id)
                    .ok_or((id, "media source is not loaded".to_owned()))?
                    .loop_enabled = enabled;
                diagnostic(format_args!("LITE_AUDIO loop id={id} enabled={enabled}"));
            }
            Command::Close { id } => {
                self.close_stream(id);
                self.media.remove(&id);
            }
            Command::GetMasterState => {
                self.ensure_service().map_err(|error| (0, error))?;
                self.send(ClientMessage::GetMasterState)
                    .map_err(|error| (0, error))?;
            }
            Command::SetMasterVolume { percent } => {
                self.ensure_service().map_err(|error| (0, error))?;
                self.send(ClientMessage::SetMasterVolume { percent })
                    .map_err(|error| (0, error))?;
            }
            Command::SetMasterMuted { muted } => {
                self.ensure_service().map_err(|error| (0, error))?;
                self.send(ClientMessage::SetMasterMuted { muted })
                    .map_err(|error| (0, error))?;
            }
        }
        Ok(())
    }

    fn ensure_service(&mut self) -> Result<(), String> {
        if self.service.is_some() {
            return Ok(());
        }
        let service = UnixStream::connect(SOCKET_PATH).map_err(|error| error.to_string())?;
        send_client(
            &service,
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                role: self.role,
            },
        )
        .map_err(|error| error.to_string())?;
        let Some((message, fd)) =
            recv_service(&service, &mut self.service_bytes).map_err(|error| error.to_string())?
        else {
            return Err("audio service closed during handshake".to_owned());
        };
        if fd.is_some()
            || message
                != (ServiceMessage::Welcome {
                    version: PROTOCOL_VERSION,
                })
        {
            return Err("audio service rejected protocol v1".to_owned());
        }
        self.service = Some(service);
        Ok(())
    }

    fn ensure_stream(&mut self, id: u64) -> Result<(), String> {
        if self
            .media
            .get(&id)
            .and_then(|media| media.stream_id)
            .is_some()
        {
            return Ok(());
        }
        self.ensure_service()?;
        let generation = self
            .media
            .get(&id)
            .ok_or_else(|| "media source is not loaded".to_owned())?
            .generation;
        self.send(ClientMessage::CreateStream { generation })?;
        let (stream_id, ring) = self.wait_for_stream_created(generation)?;
        let gain = {
            let media = self.media.get_mut(&id).expect("media was checked");
            media.stream_id = Some(stream_id);
            media.ring = Some(ring);
            media.gain
        };
        if gain != 1.0 {
            self.send(ClientMessage::SetGain {
                stream_id,
                generation,
                gain,
            })?;
        }
        Ok(())
    }

    fn fill(&mut self, id: u64) -> Result<FillResult, String> {
        let mut quantum = [0.0; RENDER_FRAMES * CHANNELS];
        let (stream_id, generation, frames, written, edge, first) = {
            let media = self
                .media
                .get_mut(&id)
                .ok_or_else(|| "media source is not loaded".to_owned())?;
            let stream_id = media
                .stream_id
                .ok_or_else(|| "audio stream is absent".to_owned())?;
            let ring = media
                .ring
                .as_mut()
                .ok_or_else(|| "audio ring is absent".to_owned())?;
            let snapshot = ring
                .snapshot()
                .map_err(|error| format!("audio ring snapshot failed: {error:?}"))?;
            if !quantum_fits(snapshot.produced_frames, snapshot.consumed_frames)? {
                return Ok(FillResult::Full);
            }
            let frames = media.decoder.read_quantum(&mut quantum)?;
            if frames == 0 {
                media.decode_eof = true;
                return Ok(FillResult::Eof);
            }
            let frame_slice = interleaved_frames(&quantum[..frames * CHANNELS]);
            let (written, edge) = ring
                .write(media.generation, frame_slice)
                .map_err(|error| format!("audio ring write failed: {error:?}"))?;
            media.submitted_frames += written as u64;
            (
                stream_id,
                media.generation,
                frames,
                written,
                edge,
                media.submitted_frames == frames as u64,
            )
        };
        if edge {
            self.send(ClientMessage::RingNonempty {
                stream_id,
                generation,
            })?;
        }
        if written != frames {
            return Err("audio ring rejected a bounded render quantum".to_owned());
        }
        if first {
            self.emit(Event::plain(id, "loadeddata"));
            self.emit(Event::plain(id, "canplay"));
        }
        Ok(FillResult::Written)
    }

    fn seek(&mut self, id: u64, seconds: f64) -> Result<(), String> {
        self.emit(Event::time(id, "seeking", seconds));
        let was_playing = self
            .media
            .get(&id)
            .map(|media| media.playing || media.starting)
            .ok_or_else(|| "media source is not loaded".to_owned())?;
        let new_generation = self.reset_timeline(id, seconds)?;
        self.emit(Event::time(id, "timeupdate", seconds));
        self.emit(Event::time(id, "seeked", seconds));
        if was_playing {
            self.prefill(id)?;
            let media = self.media.get_mut(&id).expect("media was checked");
            let stream_id = media.stream_id.expect("playing seek keeps stream");
            media.starting = true;
            self.send(ClientMessage::Start {
                stream_id,
                generation: new_generation,
            })?;
        }
        Ok(())
    }

    /// Flushes queued PCM and repositions the decoder without publishing Web seek events.
    fn reset_timeline(&mut self, id: u64, seconds: f64) -> Result<u64, String> {
        let media = self
            .media
            .get(&id)
            .ok_or_else(|| "media source is not loaded".to_owned())?;
        let old_generation = media.generation;
        let new_generation = old_generation
            .checked_add(1)
            .ok_or_else(|| "media generation exhausted".to_owned())?;
        if let Some(stream_id) = media.stream_id {
            // 1. The service drops unconfirmed PCM on pause/flush.
            // 2. Wait for its generation barrier before touching decoder state.
            // 3. Decode resumes at the last device-confirmed media position.
            self.send(ClientMessage::Flush {
                stream_id,
                old_generation,
                new_generation,
            })?;
            self.wait_for_flush(stream_id, old_generation, new_generation)?;
        }
        let media = self.media.get_mut(&id).expect("media was checked");
        media.decoder.seek(seconds)?;
        media.generation = new_generation;
        media.base_seconds = seconds;
        media.submitted_frames = 0;
        media.consumed_frames = 0;
        media.decode_eof = false;
        media.playing = false;
        media.starting = false;
        media.ending = false;
        media.resume_position = None;
        Ok(new_generation)
    }
}

fn interleaved_frames(samples: &[f32]) -> &[[f32; CHANNELS]] {
    // SAFETY: f32 arrays have identical alignment/layout to contiguous f32 and
    // the caller supplies an exact whole number of stereo frames.
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr().cast::<[f32; CHANNELS]>(),
            samples.len() / CHANNELS,
        )
    }
}

fn quantum_fits(produced_frames: u64, consumed_frames: u64) -> Result<bool, String> {
    let used = produced_frames
        .checked_sub(consumed_frames)
        .ok_or_else(|| "audio ring indices moved backwards".to_owned())?;
    Ok((RING_CAPACITY_FRAMES as u64).saturating_sub(used) >= RENDER_FRAMES as u64)
}
