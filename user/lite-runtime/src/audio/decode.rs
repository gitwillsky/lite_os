//! Streaming media decode and the single 48 kHz stereo float normalization path.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use symphonia::{
    core::{
        audio::GenericAudioBufferRef,
        codecs::audio::{AudioDecoder, AudioDecoderOptions},
        errors::Error as SymphoniaError,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType},
        io::{MediaSource, MediaSourceStream},
        meta::MetadataOptions,
        units::Time,
    },
    default::{get_codecs, get_probe},
};

pub(super) const OUTPUT_RATE: u32 = 48_000;
pub(super) const OUTPUT_CHANNELS: usize = 2;

/// Live state of one streaming download, shared between an app's HTTP worker
/// (which writes the temp file and advances `received`) and a decoder
/// [`GrowingFile`] (which blocks its reads until enough bytes have arrived).
/// The temp file at the decoder path is written strictly append-only by the
/// worker. Public streaming infrastructure: an app extension that downloads
/// while playing (e.g. the music player) creates one, spawns a worker that
/// advances these fields, and registers it via
/// [`ExtensionCx`](crate::ExtensionCx) so `media.load` of `stream:<id>` streams
/// it.
#[derive(Debug, Default)]
pub struct StreamState {
    /// Bytes written to the temp file so far (the readable high-water mark).
    pub received: u64,
    /// Content-Length when the server reported one.
    pub total: Option<u64>,
    /// Download finished (`received == total`, or EOF without Content-Length).
    pub done: bool,
    /// Terminal error, if the download failed.
    pub error: Option<String>,
    /// The UI/decoder requested cancellation.
    pub cancelled: bool,
}

/// Shared handle to a streaming download's [`StreamState`].
pub type SharedStream = Arc<Mutex<StreamState>>;

/// Metadata exposed by the HTML media state machine after container probing.
#[derive(Clone, Copy, Debug)]
pub(super) struct Metadata {
    pub(super) duration: f64,
}

/// One streaming Symphonia decoder. The input file is never copied into QuickJS memory.
pub(super) struct DecoderSession {
    path: PathBuf,
    source_offset: u64,
    source_length: u64,
    /// Present when the source is a still-downloading stream; drives read
    /// back-pressure and relaxes the finite-duration gate.
    stream: Option<SharedStream>,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    metadata: Metadata,
    resampler: SincResampler,
    output: Vec<f32>,
    output_offset: usize,
    interleaved: Vec<f32>,
    stereo: Vec<f32>,
    eof: bool,
    allocation_epoch: usize,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct WorkingCapacities {
    interleaved: usize,
    stereo: usize,
    output: usize,
    resampler_input: usize,
}

impl DecoderSession {
    /// Opens one bounded filesystem-backed Blob without copying it into memory.
    /// Convenience wrapper over [`Self::open_source`] for local (non-streaming)
    /// sources; used by the decode tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn open_range(path: &Path, offset: u64, length: u64) -> Result<Self, String> {
        Self::open_source(path, offset, length, None)
    }

    /// Opens a decoder over either a fully-present bounded file (`stream: None`,
    /// the local-playback path) or a still-downloading stream (`stream: Some`,
    /// the online-playback path). Streaming relaxes three of `BoundedFile`'s
    /// assumptions — see [`GrowingFile`] — and the finite-duration gate.
    pub(super) fn open_source(
        path: &Path,
        offset: u64,
        length: u64,
        stream: Option<SharedStream>,
    ) -> Result<Self, String> {
        let source: Box<dyn MediaSource> = match &stream {
            Some(state) => Box::new(GrowingFile::open(path, state.clone())?),
            None => Box::new(BoundedFile::open(path, offset, length)?),
        };
        let media_stream = MediaSourceStream::new(source, Default::default());
        let mut hint = symphonia::core::formats::probe::Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let format = get_probe()
            .probe(
                &hint,
                media_stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|error| error.to_string())?;
        let media_duration = format
            .media_info()
            .time_base
            .zip(format.media_info().duration)
            .and_then(|(time_base, duration)| {
                time_base
                    .calc_time(symphonia::core::units::Timestamp::new(
                        duration.get().try_into().ok()?,
                    ))
                    .map(|time| time.as_secs_f64())
            });
        let track = format
            .first_track_known_codec(TrackType::Audio)
            .ok_or_else(|| "container has no supported audio track".to_owned())?;
        let codec_params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| "audio track has no codec parameters".to_owned())?
            .clone();
        let source_rate = codec_params
            .sample_rate
            .ok_or_else(|| "audio track has no sample rate".to_owned())?;
        let duration = media_duration
            .or_else(|| {
                track
                    .time_base
                    .zip(track.duration)
                    .and_then(|(time_base, duration)| {
                        time_base
                            .calc_time(symphonia::core::units::Timestamp::new(
                                duration.get().try_into().ok()?,
                            ))
                            .map(|time| time.as_secs_f64())
                    })
            })
            .or_else(|| {
                track
                    .num_frames
                    .map(|frames| frames as f64 / f64::from(source_rate))
            })
            .unwrap_or(f64::NAN);
        // Local files must expose a finite duration up front. A still-
        // downloading stream legitimately has an unknown duration until enough
        // of the container has arrived; accept NaN there and let the media
        // machine emit `durationchange` once the decoder learns it.
        if stream.is_none() && (!duration.is_finite() || duration <= 0.0) {
            return Err("audio container has no finite positive duration".to_owned());
        }
        let decoder = get_codecs()
            .make_audio_decoder(&codec_params, &AudioDecoderOptions::default().gapless(true))
            .map_err(|error| error.to_string())?;
        let track_id = track.id;
        Ok(Self {
            path: path.to_owned(),
            source_offset: offset,
            source_length: length,
            stream,
            format,
            decoder,
            track_id,
            metadata: Metadata { duration },
            resampler: SincResampler::new(source_rate),
            output: Vec::new(),
            output_offset: 0,
            interleaved: Vec::new(),
            stereo: Vec::new(),
            eof: false,
            allocation_epoch: 0,
        })
    }

    pub(super) fn metadata(&self) -> Metadata {
        self.metadata
    }

    pub(super) fn allocation_epoch(&self) -> usize {
        self.allocation_epoch + self.resampler.allocation_epoch
    }

    #[cfg(test)]
    pub(super) fn working_capacities(&self) -> WorkingCapacities {
        WorkingCapacities {
            interleaved: self.interleaved.capacity(),
            stereo: self.stereo.capacity(),
            output: self.output.capacity(),
            resampler_input: self.resampler.input.capacity(),
        }
    }

    /// Decodes at most one 128-frame render quantum into `destination`.
    pub(super) fn read_quantum(&mut self, destination: &mut [f32]) -> Result<usize, String> {
        debug_assert_eq!(destination.len(), 128 * OUTPUT_CHANNELS);
        while self.output.len().saturating_sub(self.output_offset) < destination.len() && !self.eof
        {
            self.decode_packet()?;
        }
        let available = self.output.len().saturating_sub(self.output_offset);
        let copied = available.min(destination.len());
        destination[..copied]
            .copy_from_slice(&self.output[self.output_offset..self.output_offset + copied]);
        self.output_offset += copied;
        if self.output_offset == self.output.len() {
            self.output.clear();
            self.output_offset = 0;
        } else if self.output_offset >= 16 * 1024 {
            self.output.copy_within(self.output_offset.., 0);
            self.output.truncate(self.output.len() - self.output_offset);
            self.output_offset = 0;
        }
        Ok(copied / OUTPUT_CHANNELS)
    }

    /// Performs an accurate demux seek, resets codec history and discards to the exact target.
    pub(super) fn seek(&mut self, seconds: f64) -> Result<(), String> {
        if self.eof {
            // ISO BMFF readers finish with no pending atom. Reopen the same
            // source before seeking so replay-after-ended follows the same
            // accurate seek path instead of depending on demuxer EOF state.
            // For streams, reuse the same download handle rather than
            // restarting the download.
            let path = self.path.clone();
            *self = Self::open_source(
                &path,
                self.source_offset,
                self.source_length,
                self.stream.clone(),
            )?;
        }
        let time = Time::try_from_secs_f64(seconds)
            .ok_or_else(|| "seek target is not representable".to_owned())?;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|error| error.to_string())?;
        self.decoder.reset();
        self.resampler.reset();
        self.output.clear();
        self.output_offset = 0;
        self.eof = false;

        // Accurate mode lands no later than the target packet. Decoder reset is
        // mandatory, and decoded output before the exact requested PCM frame is
        // discarded so stale or approximate container offsets never become audible.
        let actual = self
            .format
            .tracks()
            .iter()
            .find(|track| track.id == seeked.track_id)
            .and_then(|track| track.time_base)
            .and_then(|base| base.calc_time(seeked.actual_ts))
            .map_or(0.0, |time| time.as_secs_f64());
        let mut discard_frames =
            ((seconds - actual).max(0.0) * f64::from(OUTPUT_RATE)).round() as usize;
        let mut scratch = [0.0; 128 * OUTPUT_CHANNELS];
        while discard_frames > 0 {
            let frames = self.read_quantum(&mut scratch)?;
            if frames == 0 {
                return Err("seek target is beyond decoded media".to_owned());
            }
            if frames <= discard_frames {
                discard_frames -= frames;
            } else {
                let keep_from = discard_frames * OUTPUT_CHANNELS;
                self.output
                    .extend_from_slice(&scratch[keep_from..frames * OUTPUT_CHANNELS]);
                discard_frames = 0;
            }
        }
        Ok(())
    }

    fn decode_packet(&mut self) -> Result<(), String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.eof = true;
                    self.compact_output();
                    self.resampler.finish(&mut self.output);
                    return Ok(());
                }
                Err(SymphoniaError::ResetRequired) => {
                    let path = self.path.clone();
                    *self = Self::open_source(
                        &path,
                        self.source_offset,
                        self.source_length,
                        self.stream.clone(),
                    )?;
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let capacities = (
                        self.interleaved.capacity(),
                        self.stereo.capacity(),
                        self.output.capacity(),
                    );
                    let (rate, channels) = copy_interleaved(decoded, &mut self.interleaved);
                    normalize_stereo(&self.interleaved, channels, &mut self.stereo);
                    self.resampler.set_source_rate(rate)?;
                    self.compact_output();
                    self.resampler.push(&self.stereo, &mut self.output);
                    let next = (
                        self.interleaved.capacity(),
                        self.stereo.capacity(),
                        self.output.capacity(),
                    );
                    self.allocation_epoch += usize::from(capacities.0 != next.0)
                        + usize::from(capacities.1 != next.1)
                        + usize::from(capacities.2 != next.2);
                    return Ok(());
                }
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn compact_output(&mut self) {
        if self.output_offset == 0 {
            return;
        }
        // `decode_packet` is entered only when less than one render quantum is
        // live. Reclaiming the consumed prefix here keeps one packet plus that
        // bounded residual in the existing allocation; otherwise Vec::len()
        // includes historical samples and doubles capacity during steady play.
        self.output.copy_within(self.output_offset.., 0);
        self.output.truncate(self.output.len() - self.output_offset);
        self.output_offset = 0;
    }
}

struct BoundedFile {
    file: File,
    start: u64,
    length: u64,
    position: u64,
}

impl BoundedFile {
    fn open(path: &Path, start: u64, length: u64) -> Result<Self, String> {
        let mut file = File::open(path).map_err(|error| error.to_string())?;
        let file_length = file.metadata().map_err(|error| error.to_string())?.len();
        let end = start
            .checked_add(length)
            .filter(|end| *end <= file_length)
            .ok_or_else(|| "Blob range is outside its backing file".to_owned())?;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| error.to_string())?;
        debug_assert_eq!(end - start, length);
        Ok(Self {
            file,
            start,
            length,
            position: 0,
        })
    }
}

impl Read for BoundedFile {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        let limit = destination
            .len()
            .min(remaining.try_into().unwrap_or(usize::MAX));
        let read = self.file.read(&mut destination[..limit])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for BoundedFile {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let target = match target {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.length) + i128::from(offset),
        };
        let position = u64::try_from(target)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek before Blob start"))?;
        let absolute = self
            .start
            .checked_add(position)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Blob seek overflow"))?;
        self.file.seek(SeekFrom::Start(absolute))?;
        self.position = position;
        Ok(position)
    }
}

impl MediaSource for BoundedFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

/// A media source over a temp file that the HTTP worker is still appending to.
/// Reads block (with backoff) until the requested bytes have been downloaded,
/// so Symphonia can decode progressively while the download continues. Unlike
/// [`BoundedFile`], the full byte range need not exist at open time.
struct GrowingFile {
    file: File,
    position: u64,
    stream: SharedStream,
}

impl GrowingFile {
    fn open(path: &Path, stream: SharedStream) -> Result<Self, String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
        Ok(Self {
            file,
            position: 0,
            stream,
        })
    }

    /// Snapshot of the shared download state.
    fn snapshot(&self) -> (u64, Option<u64>, bool, Option<String>) {
        self.stream
            .lock()
            .map(|state| (state.received, state.total, state.done, state.error.clone()))
            .unwrap_or((
                self.position,
                None,
                true,
                Some("stream state poisoned".into()),
            ))
    }
}

impl Read for GrowingFile {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        // Block until at least one byte past our position has been written, the
        // download finished, or it failed/was cancelled.
        loop {
            let (received, _total, done, error) = self.snapshot();
            if let Some(error) = error {
                return Err(io::Error::other(error));
            }
            if self.position < received {
                let available = received - self.position;
                let limit = destination
                    .len()
                    .min(available.try_into().unwrap_or(usize::MAX));
                let read = self.file.read(&mut destination[..limit])?;
                self.position += read as u64;
                return Ok(read);
            }
            if done {
                // At or past the final byte count: genuine EOF.
                return Ok(0);
            }
            // Not yet downloaded: yield briefly and re-poll the high-water mark.
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Seek for GrowingFile {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let (received, total, _done, _error) = self.snapshot();
        let end = total.unwrap_or(received);
        let position = match target {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => {
                u64::try_from(i128::from(self.position) + i128::from(offset)).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "stream seek before start")
                })?
            }
            SeekFrom::End(offset) => {
                u64::try_from(i128::from(end) + i128::from(offset)).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "stream seek before start")
                })?
            }
        };
        // Only seek within the already-downloaded prefix; Symphonia probes
        // early bytes and progressive formats never need to seek ahead.
        self.file.seek(SeekFrom::Start(position))?;
        self.position = position;
        Ok(position)
    }
}

impl MediaSource for GrowingFile {
    fn is_seekable(&self) -> bool {
        // Seekable only within the downloaded prefix. Reporting `true` lets
        // Symphonia rewind after probing the header (which is always present
        // once the first read succeeds); forward seeks past `received` are the
        // caller's responsibility to avoid.
        true
    }

    fn byte_len(&self) -> Option<u64> {
        // Content-Length when known; None signals an unbounded/unknown-length
        // source, which the progressive formats (MP3/FLAC/Ogg) tolerate.
        self.snapshot().1
    }
}

fn copy_interleaved(decoded: GenericAudioBufferRef<'_>, output: &mut Vec<f32>) -> (u32, usize) {
    let spec = decoded.spec();
    let channels = spec.channels().count();
    output.resize(decoded.samples_interleaved(), 0.0);
    decoded.copy_to_slice_interleaved(output);
    (spec.rate(), channels)
}

fn normalize_stereo(input: &[f32], channels: usize, output: &mut Vec<f32>) {
    output.clear();
    let required = input.len().saturating_mul(2);
    if output.capacity() < required {
        output.reserve(required);
    }
    match channels {
        0 => {}
        1 => {
            for sample in input {
                output.extend_from_slice(&[*sample, *sample]);
            }
        }
        2 => output.extend_from_slice(input),
        _ => {
            for frame in input.chunks_exact(channels) {
                // ITU-style bounded downmix for the common L/R/C/LFE/surround
                // ordering. Extra channels contribute at -3 dB and the result is
                // normalized to prevent the format adapter from clipping.
                let center = frame.get(2).copied().unwrap_or(0.0) * 0.707_106_77;
                let lfe = frame.get(3).copied().unwrap_or(0.0) * 0.5;
                let left_surround = frame.get(4).copied().unwrap_or(0.0) * 0.707_106_77;
                let right_surround = frame.get(5).copied().unwrap_or(0.0) * 0.707_106_77;
                output.extend_from_slice(&[
                    (frame[0] + center + lfe + left_surround) * 0.5,
                    (frame[1] + center + lfe + right_surround) * 0.5,
                ]);
            }
        }
    }
}

/// Fixed 32-tap windowed-sinc resampler with bounded work per output frame.
struct SincResampler {
    source_rate: u32,
    phase: f64,
    input: Vec<f32>,
    input_offset: usize,
    coefficients: Vec<[f32; 32]>,
    finished: bool,
    allocation_epoch: usize,
}

impl SincResampler {
    const HALF: isize = 16;
    const PHASES: usize = 1024;

    fn new(source_rate: u32) -> Self {
        let mut value = Self {
            source_rate,
            phase: 0.0,
            input: Vec::with_capacity((16_384 + Self::HALF as usize * 2) * OUTPUT_CHANNELS),
            input_offset: 0,
            coefficients: Vec::new(),
            finished: false,
            allocation_epoch: 0,
        };
        value.rebuild_coefficients();
        value
    }

    fn set_source_rate(&mut self, source_rate: u32) -> Result<(), String> {
        if source_rate == 0 {
            return Err("decoder produced zero sample rate".to_owned());
        }
        if self.source_rate != source_rate && !self.input.is_empty() {
            return Err("mid-stream sample-rate change is unsupported".to_owned());
        }
        if self.source_rate != source_rate {
            self.source_rate = source_rate;
            self.rebuild_coefficients();
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.input.clear();
        self.input_offset = 0;
        self.finished = false;
    }

    fn push(&mut self, stereo: &[f32], output: &mut Vec<f32>) {
        let old_capacity = self.input.capacity();
        self.input.extend_from_slice(stereo);
        self.allocation_epoch += usize::from(self.input.capacity() != old_capacity);
        self.render(output);
    }

    fn finish(&mut self, output: &mut Vec<f32>) {
        self.finished = true;
        if self.source_rate != OUTPUT_RATE {
            self.input.resize(
                self.input.len() + Self::HALF as usize * OUTPUT_CHANNELS,
                0.0,
            );
        }
        self.render(output);
    }

    fn render(&mut self, output: &mut Vec<f32>) {
        if self.source_rate == OUTPUT_RATE {
            output.extend_from_slice(&self.input[self.input_offset * OUTPUT_CHANNELS..]);
            self.input.clear();
            self.input_offset = 0;
            self.phase = 0.0;
            return;
        }
        let frames = self.input.len() / OUTPUT_CHANNELS;
        let step = f64::from(self.source_rate) / f64::from(OUTPUT_RATE);
        while (self.phase.floor() as isize + Self::HALF) < frames as isize {
            let phase_index =
                ((self.phase.fract() * Self::PHASES as f64).round() as usize) % Self::PHASES;
            let coefficients = &self.coefficients[phase_index];
            for channel in 0..OUTPUT_CHANNELS {
                let mut sample = 0.0;
                let center = self.phase.floor() as isize;
                for (tap, index) in (center - Self::HALF + 1..=center + Self::HALF).enumerate() {
                    let value = if index < 0 {
                        0.0
                    } else {
                        self.input[index as usize * OUTPUT_CHANNELS + channel] as f64
                    };
                    sample += value * f64::from(coefficients[tap]);
                }
                output.push(sample as f32);
            }
            self.phase += step;
        }
        let removable = (self.phase.floor() as isize - Self::HALF).max(0) as usize;
        self.input_offset = self.input_offset.max(removable);
        if self.input_offset >= 8192 {
            let samples = self.input_offset * OUTPUT_CHANNELS;
            self.input.copy_within(samples.., 0);
            self.input.truncate(self.input.len() - samples);
            self.phase -= self.input_offset as f64;
            self.input_offset = 0;
        }
        if self.finished {
            self.input.clear();
            self.input_offset = 0;
        }
    }

    fn rebuild_coefficients(&mut self) {
        let old_capacity = self.coefficients.capacity();
        self.coefficients.clear();
        self.coefficients.reserve(Self::PHASES);
        let cutoff = (f64::from(OUTPUT_RATE) / f64::from(self.source_rate)).min(1.0) * 0.94;
        for phase in 0..Self::PHASES {
            let fraction = phase as f64 / Self::PHASES as f64;
            let mut taps = [0.0; 32];
            let mut sum = 0.0;
            for (tap, relative) in (-Self::HALF + 1..=Self::HALF).enumerate() {
                let distance = fraction - relative as f64;
                let argument = distance * cutoff;
                let sinc = if argument.abs() < f64::EPSILON {
                    1.0
                } else {
                    let angle = std::f64::consts::PI * argument;
                    angle.sin() / angle
                };
                let window_position = distance / Self::HALF as f64;
                let window = if window_position.abs() >= 1.0 {
                    0.0
                } else {
                    0.5 + 0.5 * (std::f64::consts::PI * window_position).cos()
                };
                taps[tap] = (sinc * window * cutoff) as f32;
                sum += f64::from(taps[tap]);
            }
            for tap in &mut taps {
                *tap = (f64::from(*tap) / sum) as f32;
            }
            self.coefficients.push(taps);
        }
        self.allocation_epoch += usize::from(self.coefficients.capacity() != old_capacity);
    }
}

#[cfg(test)]
mod tests;
