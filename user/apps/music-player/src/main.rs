//! The LiteOS online music player.
//!
//! A thin binary that runs the generic `lite-runtime` GUI/JS runtime with one
//! [`MusicExt`] extension. The extension owns everything provider-specific —
//! QQ/NetEase request signing, the HTTPS+TLS stack, and the
//! play-while-downloading stream worker — so the runtime library and the other
//! app binaries carry none of it. The React UI (bundled to
//! `/usr/share/liteos/apps/music-player`) reaches these ops through the
//! `lite:net` module.
//!
//! Async ops (`net.request`, `music.search`, `music.songUrl`) return a numeric
//! request id synchronously, then a worker thread performs the blocking HTTP
//! call and publishes `{requestId, status, body, error}` on the `net` channel;
//! JS resolves a per-id promise. Streaming ops manage a growing temp file the
//! runtime's decoder plays as it fills (see [`stream`]).

mod netease;
mod provider;
mod qq;
mod stream;
mod tls;

use std::process::exit;
use std::thread;

use lite_runtime::{EngineError, ExtensionCx, ExtensionEvents, HostExtension, Role};
use serde::Deserialize;

use stream::Streams;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("music-player: invariant failure: {info}");
    }));
    if let Err(error) = lite_runtime::run(Role::App, "music-player", vec![Box::new(MusicExt::default())]) {
        eprintln!("music-player: {error}");
        exit(1);
    }
}

/// The music player's provider/network extension.
#[derive(Default)]
struct MusicExt {
    streams: Streams,
}

impl HostExtension for MusicExt {
    fn invoke(
        &mut self,
        cx: &ExtensionCx,
        operation: &str,
        payload: &str,
    ) -> Option<Result<String, EngineError>> {
        match operation {
            "net.request" => Some(self.net_request(cx, payload)),
            "music.search" => Some(self.music_search(cx, payload)),
            "music.songUrl" => Some(self.music_song_url(cx, payload)),
            "net.stream.open" => Some(self.stream_open(cx, payload)),
            "net.stream.stat" => Some(self.stream_stat(payload)),
            "net.stream.close" => Some(self.stream_close(cx, payload)),
            _ => None,
        }
    }
}

impl MusicExt {
    /// Spawns `work` on a worker thread; its `(status, body)`/error result is
    /// published on the `net` channel keyed by `request_id`. Returns the id as
    /// the synchronous op result (the JS `call()` helper awaits the event).
    fn spawn_request<F>(&self, cx: &ExtensionCx, work: F) -> Result<String, EngineError>
    where
        F: FnOnce() -> Result<(u16, String), String> + Send + 'static,
    {
        let request_id = cx.next_request_id();
        let events: ExtensionEvents = cx.events();
        thread::Builder::new()
            .name(format!("music-req-{request_id}"))
            .spawn(move || {
                let event = match work() {
                    Ok((status, body)) => serde_json::json!({
                        "requestId": request_id,
                        "status": status,
                        "body": body,
                    }),
                    Err(error) => serde_json::json!({
                        "requestId": request_id,
                        "error": error,
                    }),
                };
                events.emit("net", event);
            })
            .map_err(|error| EngineError::from_host(format!("spawn request worker: {error}")))?;
        Ok(request_id.to_string())
    }

    /// Buffered HTTP for arbitrary signed JSON API calls the UI builds itself.
    fn net_request(&self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            url: String,
            #[serde(default = "default_method")]
            method: String,
            #[serde(default)]
            headers: Vec<(String, String)>,
            #[serde(default)]
            body: String,
        }
        fn default_method() -> String {
            "GET".into()
        }
        let request: Request = json(payload)?;
        self.spawn_request(cx, move || {
            let agent = tls::agent();
            let mut builder = if request.method.eq_ignore_ascii_case("POST") {
                agent.post(&request.url)
            } else {
                agent.get(&request.url)
            };
            for (name, value) in &request.headers {
                builder = builder.set(name, value);
            }
            let response = if request.method.eq_ignore_ascii_case("POST") {
                builder.send_string(&request.body)
            } else {
                builder.call()
            };
            match response {
                Ok(response) | Err(ureq::Error::Status(_, response)) => {
                    let status = response.status();
                    response
                        .into_string()
                        .map(|body| (status, body))
                        .map_err(|error| format!("read body: {error}"))
                }
                Err(error) => Err(error.to_string()),
            }
        })
    }

    /// Signed provider search; body is the normalized `RemoteTrack[]` JSON.
    fn music_search(&self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            source: String,
            query: String,
            limit: u32,
        }
        let request: Request = json(payload)?;
        self.spawn_request(cx, move || {
            provider::search(&request.source, &request.query, request.limit)
        })
    }

    /// Resolve a playable URL for one track at one quality tier; body is
    /// `{"url": "..."|null}`.
    fn music_song_url(&self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            source: String,
            id: String,
            #[serde(default)]
            level: String,
            #[serde(default)]
            quality_index: usize,
        }
        let request: Request = json(payload)?;
        self.spawn_request(cx, move || {
            provider::song_url(&request.source, &request.id, &request.level, request.quality_index)
        })
    }

    /// Begin a streaming download; returns the numeric stream id synchronously.
    fn stream_open(&mut self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        #[derive(Deserialize)]
        struct Request {
            url: String,
            #[serde(default)]
            ext: String,
        }
        let request: Request = json(payload)?;
        let id = cx.next_request_id();
        let (path, state) = self
            .streams
            .open(id, &request.url, &request.ext, cx.events())
            .map_err(EngineError::from_host)?;
        // Register with the runtime so `media.load` of `stream:<id>` resolves
        // this growing temp file + shared download state to the decoder.
        cx.register_stream(id, path, state);
        Ok(id.to_string())
    }

    /// Synchronous progress snapshot for one stream.
    fn stream_stat(&self, payload: &str) -> Result<String, EngineError> {
        let id: u64 = payload
            .trim()
            .parse()
            .map_err(|_| EngineError::from_host("invalid stream id"))?;
        Ok(self.streams.stat(id).to_string())
    }

    /// Cancel a stream, drop its temp file, and unregister it.
    fn stream_close(&mut self, cx: &ExtensionCx, payload: &str) -> Result<String, EngineError> {
        let id: u64 = payload
            .trim()
            .parse()
            .map_err(|_| EngineError::from_host("invalid stream id"))?;
        if self.streams.close(id).is_some() {
            cx.remove_stream(id);
        }
        Ok(String::new())
    }
}

/// Parses a JSON op payload into `T`, mapping errors to `EngineError`.
fn json<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, EngineError> {
    serde_json::from_str(payload).map_err(|error| EngineError::from_host(error.to_string()))
}
