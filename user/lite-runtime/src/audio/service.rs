//! Blocking generation barriers that tolerate already-queued old-generation progress.

use std::{
    io::Write,
    time::{Duration, Instant},
};

use audio_proto::{
    AckOperation, ClientMessage, MappedProducer, RING_CAPACITY_FRAMES, SAMPLE_RATE, ServiceMessage,
    recv_service, send_client,
};

use super::{Event, FillResult, PREFILL_BLOCKS, Worker, diagnostic};

const BASE_TIMEUPDATE_INTERVAL: Duration = Duration::from_millis(250);

fn timeupdate_interval(concurrent_playbacks: u32, render_parallelism: u32) -> Duration {
    let shares = concurrent_playbacks
        .max(1)
        .div_ceil(render_parallelism.max(1));
    BASE_TIMEUPDATE_INTERVAL.saturating_mul(shares)
}

impl Worker {
    pub(super) fn send(&self, message: ClientMessage) -> Result<(), String> {
        send_client(
            self.service
                .as_ref()
                .ok_or_else(|| "audio service is disconnected".to_owned())?,
            message,
        )
        .map_err(|error| error.to_string())
    }

    fn fill_blocks(&mut self, id: u64, count: usize) -> Result<(), String> {
        for _ in 0..count {
            if !matches!(self.fill(id)?, FillResult::Written) {
                break;
            }
        }
        Ok(())
    }

    pub(super) fn prefill(&mut self, id: u64) -> Result<(), String> {
        self.fill_blocks(id, PREFILL_BLOCKS / 2)?;
        let settled_epoch = self
            .media
            .get(&id)
            .ok_or_else(|| "media source disappeared during prefill".to_owned())?
            .decoder
            .allocation_epoch();
        self.fill_blocks(id, PREFILL_BLOCKS - PREFILL_BLOCKS / 2)?;
        let final_epoch = self
            .media
            .get(&id)
            .ok_or_else(|| "media source disappeared during prefill".to_owned())?
            .decoder
            .allocation_epoch();
        let steady_allocations = final_epoch.saturating_sub(settled_epoch);
        diagnostic(format_args!(
            "LITE_AUDIO worker-allocation id={id} warmup_epoch={final_epoch} steady_allocations={steady_allocations}"
        ));
        Ok(())
    }

    fn refill_available(&mut self, id: u64) -> Result<(), String> {
        // One full-capacity bound re-arms the consumer's full-to-available edge.
        // Refilling only one device period can leave the ring partially empty
        // after scheduling delay, so no later RingAvailable notification occurs.
        self.fill_blocks(id, PREFILL_BLOCKS)
    }

    pub(super) fn wait_for_stream_created(
        &mut self,
        generation: u64,
    ) -> Result<(u64, MappedProducer), String> {
        loop {
            let service = self
                .service
                .as_ref()
                .ok_or_else(|| "audio service is disconnected".to_owned())?;
            let Some((message, fd)) = recv_service(service, &mut self.service_bytes)
                .map_err(|error| error.to_string())?
            else {
                return Err("audio service closed while creating stream".to_owned());
            };
            match message {
                ServiceMessage::StreamCreated {
                    stream_id,
                    generation: confirmed,
                    capacity_frames,
                } => {
                    if confirmed != generation
                        || capacity_frames as usize != RING_CAPACITY_FRAMES
                        || self.closing_streams.contains_key(&stream_id)
                        || self
                            .media
                            .values()
                            .any(|media| media.stream_id == Some(stream_id))
                    {
                        return Err("audio service published an incompatible stream".to_owned());
                    }
                    let ring = MappedProducer::map(
                        fd.ok_or_else(|| "stream ring descriptor is missing".to_owned())?,
                    )
                    .map_err(|error| error.to_string())?;
                    return Ok((stream_id, ring));
                }
                ServiceMessage::Error {
                    stream_id: None,
                    generation: failed,
                    code,
                } if failed == generation => {
                    if fd.is_some() {
                        return Err("audio service published an unexpected descriptor".to_owned());
                    }
                    return Err(format!("audio service rejected stream creation: {code:?}"));
                }
                message => {
                    if fd.is_some() {
                        return Err("audio service published an unexpected descriptor".to_owned());
                    }
                    if !self.owns_async_message(&message) {
                        return Err("audio service violated create-stream ordering".to_owned());
                    }
                    self.handle_service_message(message)?;
                }
            }
        }
    }

    fn owns_async_message(&self, message: &ServiceMessage) -> bool {
        match *message {
            ServiceMessage::Ack {
                stream_id,
                generation,
                ..
            }
            | ServiceMessage::Progress {
                stream_id,
                generation,
                ..
            }
            | ServiceMessage::RingAvailable {
                stream_id,
                generation,
            } => {
                self.closing_streams.get(&stream_id) == Some(&generation)
                    || self.media.values().any(|media| {
                        media.stream_id == Some(stream_id) && media.generation == generation
                    })
            }
            ServiceMessage::Error {
                stream_id: Some(stream_id),
                generation,
                ..
            } => {
                self.closing_streams.get(&stream_id) == Some(&generation)
                    || self.media.values().any(|media| {
                        media.stream_id == Some(stream_id) && media.generation == generation
                    })
            }
            ServiceMessage::Error {
                stream_id: None,
                generation: 0,
                ..
            }
            | ServiceMessage::MasterState { .. } => true,
            ServiceMessage::Welcome { .. }
            | ServiceMessage::StreamCreated { .. }
            | ServiceMessage::Flushed { .. }
            | ServiceMessage::Error {
                stream_id: None, ..
            } => false,
        }
    }

    pub(super) fn wait_for_flush(
        &mut self,
        stream_id: u64,
        old_generation: u64,
        new_generation: u64,
    ) -> Result<(), String> {
        loop {
            let service = self
                .service
                .as_ref()
                .ok_or_else(|| "audio service is disconnected".to_owned())?;
            let Some((message, fd)) = recv_service(service, &mut self.service_bytes)
                .map_err(|error| error.to_string())?
            else {
                return Err("audio service closed during seek flush".to_owned());
            };
            if fd.is_some() {
                return Err("audio service published an unexpected descriptor".to_owned());
            }
            match message {
                ServiceMessage::Flushed {
                    stream_id: confirmed,
                    generation,
                } if confirmed == stream_id && generation == new_generation => {
                    while let Some(message) = self.deferred_service.pop_front() {
                        self.handle_service_message(message)?;
                    }
                    return Ok(());
                }
                ServiceMessage::Progress {
                    stream_id: progress_stream,
                    generation,
                    consumed_frames,
                    ..
                } if progress_stream == stream_id && generation == old_generation => {
                    if let Some(media) = self
                        .media
                        .values_mut()
                        .find(|media| media.stream_id == Some(stream_id))
                    {
                        media.consumed_frames = consumed_frames;
                    }
                }
                ServiceMessage::RingAvailable {
                    stream_id: progress_stream,
                    generation,
                } if progress_stream == stream_id && generation == old_generation => {}
                ServiceMessage::Ack {
                    stream_id: ack_stream,
                    generation,
                    ..
                } if ack_stream == stream_id && generation == old_generation => {}
                ServiceMessage::Error {
                    stream_id: failed_stream,
                    generation,
                    code,
                } if failed_stream.is_none_or(|failed_stream| failed_stream == stream_id)
                    && (generation == 0
                        || generation == old_generation
                        || generation == new_generation) =>
                {
                    return Err(format!("audio service rejected seek: {code:?}"));
                }
                ServiceMessage::Welcome { .. }
                | ServiceMessage::StreamCreated { .. }
                | ServiceMessage::Flushed { .. } => {
                    return Err("audio service violated seek generation ordering".to_owned());
                }
                message => self.deferred_service.push_back(message),
            }
        }
    }

    pub(super) fn close_stream(&mut self, id: u64) {
        let Some(media) = self.media.get_mut(&id) else {
            return;
        };
        let stream = media
            .stream_id
            .take()
            .map(|stream_id| (stream_id, media.generation));
        media.ring = None;
        media.playing = false;
        media.starting = false;
        media.ending = false;
        media.resume_position = None;
        if let Some((stream_id, generation)) = stream
            && self
                .send(ClientMessage::Close {
                    stream_id,
                    generation,
                })
                .is_ok()
        {
            self.closing_streams.insert(stream_id, generation);
        }
    }

    pub(super) fn disconnect_service(&mut self, error: String) {
        self.service = None;
        self.closing_streams.clear();
        let affected: Vec<_> = self
            .media
            .iter_mut()
            .filter_map(|(&id, media)| {
                let affected = media.stream_id.take().is_some();
                media.ring = None;
                media.playing = false;
                media.starting = false;
                media.ending = false;
                media.resume_position = None;
                let time =
                    media.base_seconds + media.consumed_frames as f64 / f64::from(SAMPLE_RATE);
                affected.then_some((id, time))
            })
            .collect();
        for (id, time) in affected {
            if let Some(media) = self.media.get_mut(&id) {
                media.generation = media.generation.saturating_add(1);
                media.base_seconds = time;
                media.submitted_frames = 0;
                media.consumed_frames = 0;
                media.decode_eof = false;
                media.resume_position = Some(time);
            }
            self.emit(Event::time(id, "timeupdate", time));
            self.emit(Event::plain(id, "pause"));
            self.emit(Event::plain(id, "abort"));
            self.emit(Event::error(
                id,
                3,
                format!("audio service disconnected: {error}"),
            ));
        }
    }

    pub(super) fn fail_all(&mut self, error: String) {
        let ids: Vec<_> = self.media.keys().copied().collect();
        for id in ids {
            self.emit(Event::error(id, 3, error.clone()));
        }
    }

    pub(super) fn emit(&mut self, event: Event) {
        match event.kind {
            "seeking" => diagnostic(format_args!(
                "LITE_AUDIO event=seeking id={} seconds={:.6}",
                event.id,
                event.current_time.unwrap_or_default()
            )),
            "error" => {
                if let Some(error) = &event.error {
                    diagnostic(format_args!(
                        "LITE_AUDIO event=error id={} code={} message={:?}",
                        event.id, error.code, error.message
                    ));
                } else {
                    diagnostic(format_args!("LITE_AUDIO event=error id={}", event.id));
                }
            }
            "loadedmetadata" | "play" | "playing" | "pause" | "seeked" | "ended" | "abort" => {
                diagnostic(format_args!(
                    "LITE_AUDIO event={} id={}",
                    event.kind, event.id
                ));
            }
            _ => {}
        }
        if let Ok(mut queue) = self.events.lock() {
            queue.push_back(event);
        }
        let _ = self.event_wake.write(&[1]);
    }

    pub(super) fn receive_service(&mut self) -> Result<(), String> {
        let service = self
            .service
            .as_ref()
            .ok_or_else(|| "audio service is absent".to_owned())?;
        let Some((message, fd)) =
            recv_service(service, &mut self.service_bytes).map_err(|error| error.to_string())?
        else {
            return Err("audio service closed the control connection".to_owned());
        };
        if fd.is_some() {
            return Err("unexpected audio service descriptor".to_owned());
        }
        self.handle_service_message(message)
    }

    pub(super) fn handle_service_message(&mut self, message: ServiceMessage) -> Result<(), String> {
        match message {
            ServiceMessage::Ack {
                stream_id,
                generation,
                operation,
            } => {
                if self.closing_streams.get(&stream_id) == Some(&generation) {
                    if operation == AckOperation::Close {
                        self.closing_streams.remove(&stream_id);
                    }
                    return Ok(());
                }
                let Some((&id, _)) = self.media.iter().find(|(_, media)| {
                    media.stream_id == Some(stream_id) && media.generation == generation
                }) else {
                    return Ok(());
                };
                match operation {
                    AckOperation::Start => {
                        let media = self.media.get_mut(&id).expect("matched media");
                        media.starting = false;
                        media.ending = false;
                        media.playing = true;
                        self.emit(Event::plain(id, "playing"));
                    }
                    AckOperation::Pause => {
                        let time = {
                            let media = self.media.get_mut(&id).expect("matched media");
                            if media.ending {
                                media.ending = false;
                                return Ok(());
                            }
                            media.starting = false;
                            media.playing = false;
                            let time = media.base_seconds
                                + media.consumed_frames as f64 / f64::from(SAMPLE_RATE);
                            media.resume_position = Some(time);
                            time
                        };
                        self.emit(Event::time(id, "timeupdate", time));
                        self.emit(Event::plain(id, "pause"));
                    }
                    AckOperation::Gain => {
                        let gain = self.media.get(&id).expect("matched media").gain;
                        diagnostic(format_args!(
                            "LITE_AUDIO gain-installed id={id} gain={gain:.6}"
                        ));
                    }
                    AckOperation::Close => {}
                }
            }
            ServiceMessage::Progress {
                stream_id,
                generation,
                consumed_frames,
                concurrent_playbacks,
            } => {
                if self.closing_streams.get(&stream_id) == Some(&generation) {
                    return Ok(());
                }
                let Some((&id, _)) = self.media.iter().find(|(_, media)| {
                    media.stream_id == Some(stream_id) && media.generation == generation
                }) else {
                    return Ok(());
                };
                let (timeupdate, decode_eof, submitted_frames, loop_enabled, end) = {
                    let media = self.media.get_mut(&id).expect("matched media");
                    media.consumed_frames = consumed_frames;
                    let now = Instant::now();
                    let timeupdate = if now.duration_since(media.last_timeupdate)
                        >= timeupdate_interval(concurrent_playbacks, self.render_parallelism)
                    {
                        media.last_timeupdate = now;
                        Some(media.base_seconds + consumed_frames as f64 / f64::from(SAMPLE_RATE))
                    } else {
                        None
                    };
                    (
                        timeupdate,
                        media.decode_eof,
                        media.submitted_frames,
                        media.loop_enabled,
                        media.base_seconds + media.submitted_frames as f64 / f64::from(SAMPLE_RATE),
                    )
                };
                if let Some(time) = timeupdate {
                    self.emit(Event::time(id, "timeupdate", time));
                }
                if decode_eof && consumed_frames >= submitted_frames {
                    if loop_enabled {
                        self.seek(id, 0.0)?;
                    } else {
                        let (stream_id, generation) = {
                            let media = self.media.get_mut(&id).expect("matched media");
                            media.playing = false;
                            media.ending = true;
                            media.resume_position = Some(0.0);
                            (media.stream_id.expect("live stream"), media.generation)
                        };
                        self.send(ClientMessage::Pause {
                            stream_id,
                            generation,
                        })?;
                        self.emit(Event::time(id, "timeupdate", end));
                        self.emit(Event::time(id, "ended", end));
                    }
                }
            }
            ServiceMessage::RingAvailable {
                stream_id,
                generation,
            } => {
                if self.closing_streams.get(&stream_id) == Some(&generation) {
                    return Ok(());
                }
                if let Some((&id, _)) = self.media.iter().find(|(_, media)| {
                    media.stream_id == Some(stream_id) && media.generation == generation
                }) {
                    self.refill_available(id)?;
                }
            }
            ServiceMessage::Error {
                stream_id,
                generation,
                code,
            } => {
                if stream_id.is_some_and(|stream_id| {
                    self.closing_streams.get(&stream_id) == Some(&generation)
                }) {
                    return Err(format!("audio service rejected closing stream: {code:?}"));
                }
                if let Some((&id, media)) = self.media.iter_mut().find(|(_, media)| {
                    stream_id.is_none_or(|stream_id| media.stream_id == Some(stream_id))
                        && (generation == 0 || media.generation == generation)
                }) {
                    media.playing = false;
                    self.emit(Event::error(
                        id,
                        3,
                        format!("audio service error: {code:?}"),
                    ));
                }
            }
            ServiceMessage::MasterState { percent, muted } => {
                self.emit(Event {
                    percent: Some(percent),
                    muted: Some(muted),
                    ..Event::plain(0, "masterstate")
                });
            }
            ServiceMessage::Welcome { .. }
            | ServiceMessage::StreamCreated { .. }
            | ServiceMessage::Flushed { .. } => {
                return Err("audio service message arrived out of order".to_owned());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod cadence_tests {
    use std::time::Duration;

    use super::timeupdate_interval;

    #[test]
    fn system_load_is_shared_across_available_render_parallelism() {
        assert_eq!(timeupdate_interval(1, 1), Duration::from_millis(250));
        assert_eq!(timeupdate_interval(8, 1), Duration::from_millis(2_000));
        assert_eq!(timeupdate_interval(8, 4), Duration::from_millis(500));
        assert_eq!(timeupdate_interval(8, 8), Duration::from_millis(250));
        assert_eq!(timeupdate_interval(0, 0), Duration::from_millis(250));
    }
}
