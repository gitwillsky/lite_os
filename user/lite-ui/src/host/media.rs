//! Native file/media operations and desktop-only system-volume capability.

use quickjs_runtime::{EngineError, Role};
use serde::Deserialize;

use crate::audio::Command as AudioCommand;

use super::{Host, filesystem::FileRange};

impl Host {
    pub(super) fn invoke_media(
        &self,
        operation: &str,
        payload: &str,
    ) -> Option<Result<String, EngineError>> {
        let result = match operation {
            "fs.open" if self.role == Role::App => self
                .files
                .borrow()
                .open(payload)
                .map_err(EngineError::from_host),
            "fs.object-url-create" if self.role == Role::App => self.create_object_url(payload),
            "fs.object-url-revoke" if self.role == Role::App => self.revoke_object_url(payload),
            "fs.read-range" if self.role == Role::App => self.read_file_range(payload),
            "media.create" => self.create_media(),
            "media.can-play-type" => self.media_can_play_type(payload),
            "media.load" => self.load_media(payload),
            "media.unload" => self.unload_media(payload),
            "media.play" => self.play_media(payload),
            "media.pause" => self.simple_media(payload, |id| AudioCommand::Pause { id }),
            "media.seek" => self.seek_media(payload),
            "media.gain" => self.gain_media(payload),
            "media.loop" => self.loop_media(payload),
            "media.close" => self.close_media(payload),
            "audio-system.get" if self.role == Role::Desktop => self
                .send_audio(AudioCommand::GetMasterState)
                .map(|()| String::new()),
            "audio-system.volume" if self.role == Role::Desktop => {
                let percent = payload
                    .parse::<u8>()
                    .ok()
                    .filter(|percent| *percent <= 100)
                    .ok_or_else(|| EngineError::from_host("invalid master volume percentage"));
                percent.and_then(|percent| {
                    self.send_audio(AudioCommand::SetMasterVolume { percent })
                        .map(|()| String::new())
                })
            }
            "audio-system.muted" if self.role == Role::Desktop => {
                let muted = match payload {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err(EngineError::from_host("invalid master mute state")),
                };
                muted.and_then(|muted| {
                    self.send_audio(AudioCommand::SetMasterMuted { muted })
                        .map(|()| String::new())
                })
            }
            _ if operation.starts_with("media.")
                || operation.starts_with("audio-system.")
                || matches!(
                    operation,
                    "fs.open" | "fs.object-url-create" | "fs.object-url-revoke" | "fs.read-range"
                ) =>
            {
                Err(EngineError::from_host(format!(
                    "operation '{operation}' is unavailable in this session"
                )))
            }
            _ => return None,
        };
        Some(result)
    }

    fn require_media(&self, id: u64) -> Result<(), EngineError> {
        self.media
            .borrow()
            .contains(&id)
            .then_some(())
            .ok_or_else(|| EngineError::from_host("unknown media element"))
    }

    fn send_audio(&self, command: AudioCommand) -> Result<(), EngineError> {
        self.audio
            .borrow_mut()
            .send(command)
            .map_err(EngineError::from_host)
    }

    fn resolve_media_source(&self, source: &str) -> Result<FileRange, EngineError> {
        if source.starts_with("blob:") {
            return self
                .files
                .borrow()
                .resolve_blob(source)
                .map_err(EngineError::from_host);
        }
        if source.is_empty()
            || source.starts_with('/')
            || source.contains("://")
            || source.split('/').any(|component| component == "..")
        {
            return Err(EngineError::from_host("unsupported media source URL"));
        }
        let path = self.app_root.join(source);
        if !path.is_file() || !path.starts_with(&self.app_root) {
            return Err(EngineError::from_host("media resource does not exist"));
        }
        let length = path
            .metadata()
            .map_err(|error| EngineError::from_host(error.to_string()))?
            .len();
        Ok(FileRange {
            path,
            offset: 0,
            length,
        })
    }

    fn create_object_url(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            path: String,
            offset: u64,
            length: u64,
        }
        let request: Request = json(payload)?;
        self.files
            .borrow_mut()
            .create_object_url(&request.path, request.offset, request.length)
            .map_err(EngineError::from_host)
    }

    fn revoke_object_url(&self, payload: &str) -> Result<String, EngineError> {
        self.files
            .borrow_mut()
            .revoke_object_url(payload)
            .map_err(EngineError::from_host)?;
        Ok(String::new())
    }

    fn read_file_range(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            path: String,
            offset: u64,
            length: usize,
        }
        let request: Request = json(payload)?;
        let bytes = self
            .files
            .borrow()
            .read_range(&request.path, request.offset, request.length)
            .map_err(EngineError::from_host)?;
        serde_json::to_string(&bytes).map_err(|error| EngineError::from_host(error.to_string()))
    }

    fn create_media(&self) -> Result<String, EngineError> {
        let id = self.next_media.get();
        self.next_media.set(
            id.checked_add(1)
                .ok_or_else(|| EngineError::from_host("media identity exhausted"))?,
        );
        self.media.borrow_mut().insert(id);
        Ok(id.to_string())
    }

    fn media_can_play_type(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            r#type: String,
        }
        let request: Request = json(payload)?;
        Ok(can_play_type(&request.r#type).to_owned())
    }

    fn load_media(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            id: u64,
            src: String,
        }
        let request: Request = json(payload)?;
        self.require_media(request.id)?;
        let source = self.resolve_media_source(&request.src)?;
        self.send_audio(AudioCommand::Load {
            id: request.id,
            path: source.path,
            offset: source.offset,
            length: source.length,
        })?;
        Ok(String::new())
    }

    fn unload_media(&self, payload: &str) -> Result<String, EngineError> {
        let id = json_media_id(payload)?;
        self.require_media(id)?;
        self.send_audio(AudioCommand::Close { id })?;
        Ok(String::new())
    }

    fn play_media(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            id: u64,
            muted: bool,
        }
        let request: Request = json(payload)?;
        self.require_media(request.id)?;
        if !request.muted && !self.state.playback_granted.get() {
            return Err(EngineError::from_host(
                "NotAllowedError: audible playback requires user activation",
            ));
        }
        self.send_audio(AudioCommand::Play { id: request.id })?;
        Ok(String::new())
    }

    fn simple_media(
        &self,
        payload: &str,
        command: impl FnOnce(u64) -> AudioCommand,
    ) -> Result<String, EngineError> {
        let id = json_media_id(payload)?;
        self.require_media(id)?;
        self.send_audio(command(id))?;
        Ok(String::new())
    }

    fn seek_media(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            id: u64,
            time: f64,
        }
        let request: Request = json(payload)?;
        self.require_media(request.id)?;
        if !request.time.is_finite() || request.time < 0.0 {
            return Err(EngineError::from_host("invalid media seek"));
        }
        self.send_audio(AudioCommand::Seek {
            id: request.id,
            seconds: request.time,
        })?;
        Ok(String::new())
    }

    fn gain_media(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            id: u64,
            volume: f32,
            muted: bool,
        }
        let request: Request = json(payload)?;
        self.require_media(request.id)?;
        if !request.volume.is_finite() || !(0.0..=1.0).contains(&request.volume) {
            return Err(EngineError::from_host("invalid media gain"));
        }
        if !request.muted && !self.state.playback_granted.get() {
            return Err(EngineError::from_host(
                "NotAllowedError: audible playback requires user activation",
            ));
        }
        self.send_audio(AudioCommand::Gain {
            id: request.id,
            gain: if request.muted { 0.0 } else { request.volume },
        })?;
        Ok(String::new())
    }

    fn loop_media(&self, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            id: u64,
            r#loop: bool,
        }
        let request: Request = json(payload)?;
        self.require_media(request.id)?;
        self.send_audio(AudioCommand::Loop {
            id: request.id,
            enabled: request.r#loop,
        })?;
        Ok(String::new())
    }

    fn close_media(&self, payload: &str) -> Result<String, EngineError> {
        let id = json_media_id(payload)?;
        self.require_media(id)?;
        self.media.borrow_mut().remove(&id);
        self.send_audio(AudioCommand::Close { id })?;
        Ok(String::new())
    }
}

fn json<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, EngineError> {
    serde_json::from_str(payload).map_err(|error| EngineError::from_host(error.to_string()))
}

fn json_media_id(payload: &str) -> Result<u64, EngineError> {
    #[derive(Deserialize)]
    struct Request {
        id: u64,
    }
    json::<Request>(payload).map(|request| request.id)
}

fn can_play_type(source: &str) -> &'static str {
    let mut fields = source.split(';');
    let media_type = fields
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let codec = fields.find_map(|field| {
        let (name, value) = field.trim().split_once('=')?;
        name.eq_ignore_ascii_case("codecs").then(|| {
            value
                .trim()
                .trim_matches('"')
                .split(',')
                .map(|codec| codec.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
    });
    let supported_codec = |supported: &[&str]| {
        codec.as_ref().is_none_or(|codecs| {
            !codecs.is_empty()
                && codecs
                    .iter()
                    .all(|codec| supported.iter().any(|candidate| codec == candidate))
        })
    };
    let supported = match media_type.as_str() {
        "audio/ogg" => supported_codec(&["vorbis"]),
        "audio/webm" => supported_codec(&["vorbis"]),
        "audio/x-matroska" => supported_codec(&[
            "vorbis",
            "flac",
            "mp1",
            "mp2",
            "mp3",
            "mp4a.40.2",
            "mp4a.40.02",
            "alac",
        ]),
        "audio/mp4" | "audio/x-m4a" => supported_codec(&["mp4a.40.2", "mp4a.40.02", "alac"]),
        "audio/mpeg" => supported_codec(&["mp1", "mp2", "mp3"]),
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/aiff" | "audio/x-aiff" => {
            supported_codec(&["1", "pcm", "adpcm"])
        }
        "audio/x-caf" => supported_codec(&[
            "1",
            "pcm",
            "adpcm",
            "flac",
            "mp1",
            "mp2",
            "mp3",
            "mp4a.40.2",
            "mp4a.40.02",
            "alac",
        ]),
        "audio/flac" | "audio/x-flac" => supported_codec(&["flac"]),
        _ => false,
    };
    if !supported {
        ""
    } else if codec.is_some() {
        "probably"
    } else {
        "maybe"
    }
}

#[cfg(test)]
mod tests {
    use super::can_play_type;

    #[test]
    fn capability_is_container_and_codec_exact() {
        assert_eq!(can_play_type(r#"audio/ogg; codecs="vorbis""#), "probably");
        assert_eq!(can_play_type(r#"audio/ogg; codecs="opus""#), "");
        assert_eq!(can_play_type(r#"audio/webm; codecs="vorbis""#), "probably");
        assert_eq!(can_play_type(r#"audio/webm; codecs="opus""#), "");
        assert_eq!(
            can_play_type(r#"audio/mp4; codecs="mp4a.40.2""#),
            "probably"
        );
        assert_eq!(can_play_type(r#"audio/mp4; codecs="alac""#), "probably");
        assert_eq!(can_play_type(r#"audio/mp4; codecs="mp4a.40.5""#), "");
        assert_eq!(can_play_type(r#"audio/mpeg; codecs="mp1""#), "probably");
        assert_eq!(can_play_type(r#"audio/mpeg; codecs="mp2""#), "probably");
        assert_eq!(can_play_type(r#"audio/mpeg; codecs="mp3""#), "probably");
        assert_eq!(can_play_type(r#"audio/flac; codecs="flac""#), "probably");
        assert_eq!(can_play_type(r#"audio/wav; codecs="pcm""#), "probably");
        assert_eq!(can_play_type(r#"audio/aiff; codecs="pcm""#), "probably");
        assert_eq!(can_play_type(r#"audio/x-caf; codecs="alac""#), "probably");
        assert_eq!(
            can_play_type(r#"audio/x-matroska; codecs="flac""#),
            "probably"
        );
        assert_eq!(can_play_type("audio/mp4"), "maybe");
        assert_eq!(can_play_type("video/mp4"), "");
    }
}
