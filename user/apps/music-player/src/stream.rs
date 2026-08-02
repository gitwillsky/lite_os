//! Play-while-downloading stream worker.
//!
//! `net.stream.open` starts a background thread that downloads a URL into a
//! temp file, appending bytes and advancing a shared [`StreamState`] as they
//! arrive. The runtime's `GrowingFile` decoder reads the same temp file, its
//! reads gated on `received`, so playback starts before the download finishes.
//!
//! Progress is reported to JavaScript on the `net` channel as
//! `{streamId, received, total, done, error}` events; `net.stream.stat`
//! returns the same snapshot synchronously. The temp file is created
//! synchronously before `open` returns so the decoder can open it immediately.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use lite_runtime::{ExtensionEvents, SharedStream, StreamState};

use crate::tls;

/// One active streaming download, tracked by the extension so `stat`/`close`
/// can reach its shared state and temp path.
pub(crate) struct Stream {
    pub path: PathBuf,
    pub state: SharedStream,
}

/// The extension's registry of in-flight streams, keyed by stream id.
#[derive(Default)]
pub(crate) struct Streams {
    map: HashMap<u64, Stream>,
}

impl Streams {
    /// Starts downloading `url` into a fresh temp file and spawns the worker.
    /// Returns the shared state + path so the caller can register the stream
    /// with the runtime (for `media.load` of `stream:<id>`).
    pub(crate) fn open(
        &mut self,
        id: u64,
        url: &str,
        ext: &str,
        events: ExtensionEvents,
    ) -> Result<(PathBuf, SharedStream), String> {
        let path = temp_path(id, ext);
        // Create the (empty) temp file synchronously so the decoder's
        // `GrowingFile::open` never races the worker's first write.
        File::create(&path).map_err(|error| format!("create temp file: {error}"))?;
        let state: SharedStream = Arc::new(Mutex::new(StreamState::default()));
        self.map.insert(
            id,
            Stream {
                path: path.clone(),
                state: state.clone(),
            },
        );
        let worker_state = state.clone();
        let worker_path = path.clone();
        let url = url.to_owned();
        thread::Builder::new()
            .name(format!("music-stream-{id}"))
            .spawn(move || download(id, &url, &worker_path, worker_state, events))
            .map_err(|error| format!("spawn stream worker: {error}"))?;
        Ok((path, state))
    }

    /// Current snapshot for `net.stream.stat`.
    pub(crate) fn stat(&self, id: u64) -> serde_json::Value {
        match self.map.get(&id) {
            Some(stream) => {
                let state = stream.state.lock().expect("stream state");
                serde_json::json!({
                    "received": state.received,
                    "total": state.total,
                    "done": state.done,
                    "error": state.error,
                })
            }
            None => serde_json::json!({ "error": "unknown stream id" }),
        }
    }

    /// Cancels a stream (the worker observes `cancelled` and stops) and drops
    /// its temp file. Returns the temp path so the caller can also unregister
    /// it from the runtime.
    pub(crate) fn close(&mut self, id: u64) -> Option<PathBuf> {
        let stream = self.map.remove(&id)?;
        if let Ok(mut state) = stream.state.lock() {
            state.cancelled = true;
        }
        let _ = std::fs::remove_file(&stream.path);
        Some(stream.path)
    }
}

/// A per-stream temp file path under the app cache directory.
fn temp_path(id: u64, ext: &str) -> PathBuf {
    let ext = if ext.is_empty() { "bin" } else { ext };
    PathBuf::from("/tmp").join(format!("music-stream-{id}.{ext}"))
}

/// Downloads `url` into `path`, advancing `state.received` and emitting
/// progress events. On any failure it records `state.error` and emits it.
fn download(id: u64, url: &str, path: &PathBuf, state: SharedStream, events: ExtensionEvents) {
    if let Err(error) = download_inner(id, url, path, &state, &events) {
        if let Ok(mut guard) = state.lock() {
            if guard.error.is_none() && !guard.cancelled {
                guard.error = Some(error.clone());
            }
        }
        events.emit("net", serde_json::json!({ "streamId": id, "error": error }));
    }
}

fn download_inner(
    id: u64,
    url: &str,
    path: &PathBuf,
    state: &SharedStream,
    events: &ExtensionEvents,
) -> Result<(), String> {
    let agent = tls::agent();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("stream request: {error}"))?;
    let total: Option<u64> = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok());
    if let Ok(mut guard) = state.lock() {
        guard.total = total;
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| format!("open temp file: {error}"))?;
    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    let mut last_emit: u64 = 0;
    loop {
        // Honor a cancellation from `net.stream.close`.
        if state.lock().map(|guard| guard.cancelled).unwrap_or(true) {
            return Ok(());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("stream read: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("stream write: {error}"))?;
        file.flush()
            .map_err(|error| format!("stream flush: {error}"))?;
        received += read as u64;
        if let Ok(mut guard) = state.lock() {
            guard.received = received;
        }
        // Throttle progress events to ~every 128 KiB to avoid flooding the bus.
        if received - last_emit >= 128 * 1024 {
            last_emit = received;
            events.emit(
                "net",
                serde_json::json!({
                    "streamId": id,
                    "received": received,
                    "total": total,
                    "done": false,
                }),
            );
        }
    }
    if let Ok(mut guard) = state.lock() {
        guard.done = true;
        guard.received = received;
    }
    events.emit(
        "net",
        serde_json::json!({
            "streamId": id,
            "received": received,
            "total": total,
            "done": true,
        }),
    );
    Ok(())
}
