//! App-provided native capability extension seam.
//!
//! The generic runtime library exposes only universal ops (scene/timer/
//! viewport/clipboard/fs/audio-playback). Each app binary supplies its own
//! native ops — desktop shell, terminal PTY, music provider/network — as a
//! [`HostExtension`], layered onto the host's dispatch cascade after the
//! built-in `invoke_media` stage.
//!
//! An extension has two coupled halves, mirroring how the audio worker splits
//! into a command side (moved into the host) and an event side (polled by the
//! run loop):
//! - **invoke**: synchronous op handling, called from `NativeHost::invoke`.
//! - **worker events**: an extension that spawns a background worker (e.g. a
//!   music download) publishes results by pushing `(channel, payload)` into the
//!   shared [`ExtensionEvents`] queue and waking the run loop over its fd. The
//!   run loop drains that queue and dispatches each event to JavaScript.

use std::collections::VecDeque;
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use quickjs_runtime::EngineError;

use crate::audio::SharedStream;
use crate::host::Action;

/// Shared queue of deferred native side effects. Held by both the runtime
/// [`State`](crate::host::State) (drained by the run loop's `process_actions`)
/// and each [`ExtensionCx`] (an extension pushes an [`Action`] to request a
/// compositor/launch/shutdown side effect). Single-threaded `Rc<RefCell<..>>`.
pub(crate) type ActionQueue = Rc<std::cell::RefCell<Vec<Action>>>;

/// Shared registry of play-while-downloading streams, held by both the runtime
/// [`State`](crate::host::State) (which resolves `stream:<id>` for `media.load`)
/// and each [`ExtensionCx`] (through which an extension registers/removes its
/// downloads). Single-threaded `Rc<RefCell<..>>`: only the JS/host thread
/// touches the map; worker threads mutate the per-stream [`SharedStream`] state,
/// not this index.
pub(crate) type StreamRegistry =
    Rc<std::cell::RefCell<std::collections::HashMap<u64, (PathBuf, SharedStream)>>>;

/// Shared, thread-safe queue an extension's worker threads push events into,
/// paired with an edge-triggered wake fd the run loop polls. This is the ONLY
/// channel through which off-thread work reaches JavaScript, so it carries no
/// app-specific type — just a channel name and a JSON payload.
#[derive(Clone)]
pub struct ExtensionEvents {
    queue: Arc<Mutex<VecDeque<(String, serde_json::Value)>>>,
    wake: Arc<Mutex<UnixStream>>,
}

impl ExtensionEvents {
    /// Publishes one `(channel, payload)` event and wakes the run loop.
    pub fn emit(&self, channel: impl Into<String>, payload: serde_json::Value) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back((channel.into(), payload));
        }
        if let Ok(mut wake) = self.wake.lock() {
            use std::io::Write;
            let _ = wake.write_all(&[1]);
        }
    }
}

/// Context handed to an extension at construction: the generic runtime
/// capabilities it may use, with no app semantics. Extensions allocate async
/// request ids here and obtain the [`ExtensionEvents`] handle to report back.
pub struct ExtensionCx {
    events: ExtensionEvents,
    next_request: Rc<std::cell::Cell<u64>>,
    streams: StreamRegistry,
    actions: ActionQueue,
    /// The installed-application registry root, for the desktop launcher.
    apps_root: PathBuf,
}

impl ExtensionCx {
    /// The event sink an extension hands to its worker threads.
    pub fn events(&self) -> ExtensionEvents {
        self.events.clone()
    }

    /// Allocates a fresh monotonic request id (for the async requestId+event
    /// pattern the JS side resolves against).
    pub fn next_request_id(&self) -> u64 {
        let id = self.next_request.get();
        self.next_request.set(id.wrapping_add(1).max(1));
        id
    }

    /// Registers a play-while-downloading stream so `media.load` of
    /// `stream:<id>` resolves to this temp file and shared download state.
    pub fn register_stream(&self, id: u64, path: PathBuf, state: SharedStream) {
        self.streams.borrow_mut().insert(id, (path, state));
    }

    /// Drops a registered stream (download finished or cancelled).
    pub fn remove_stream(&self, id: u64) {
        self.streams.borrow_mut().remove(&id);
    }

    /// Queues one deferred native side effect (launch / shutdown / compositor
    /// op) for the run loop to execute after this JavaScript turn.
    pub fn push_action(&self, action: Action) {
        self.actions.borrow_mut().push(action);
    }

    /// The installed-application registry root the desktop launcher scans.
    pub fn apps_root(&self) -> &std::path::Path {
        &self.apps_root
    }
}

/// An app-provided set of native ops. Returned `None` from [`invoke`] means the
/// op is not this extension's, so the host continues its dispatch cascade.
pub trait HostExtension {
    /// Handles one native op; `None` = not mine, keep cascading.
    fn invoke(
        &mut self,
        cx: &ExtensionCx,
        operation: &str,
        payload: &str,
    ) -> Option<Result<String, EngineError>>;
}

/// The run-loop side of the extension event channel: owns the poll fd and
/// drains queued events. Held inside the shared [`State`](crate::host::State)
/// so the event loop can poll and drain it through `&State`. All methods take
/// `&self` — the wake fd is read through `&UnixStream` (which implements
/// `Read`), and the queue is behind its own lock.
pub(crate) struct ExtensionEventLoop {
    queue: Arc<Mutex<VecDeque<(String, serde_json::Value)>>>,
    wake: UnixStream,
    /// True once at least one extension exists; a process with no extensions
    /// never registers the fd in the poll set.
    active: bool,
}

impl ExtensionEventLoop {
    /// The pollable wake fd, present only when the process has extensions.
    pub(crate) fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.active.then(|| self.wake.as_fd())
    }

    /// Drains the edge-triggered wake bytes and returns all queued events.
    pub(crate) fn drain(&self) -> Vec<(String, serde_json::Value)> {
        use std::io::Read;
        let mut bytes = [0u8; 256];
        loop {
            // `&UnixStream: Read`, so draining the non-blocking wake pipe needs
            // only a shared borrow — matching `State`'s `&self` methods.
            match (&self.wake).read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        self.queue
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

/// Builds the paired context (given to each extension) and run-loop drainer.
/// `active` reflects whether any extension was supplied.
pub(crate) fn extension_channel(
    active: bool,
    next_request: Rc<std::cell::Cell<u64>>,
    streams: StreamRegistry,
    actions: ActionQueue,
    apps_root: PathBuf,
) -> std::io::Result<(ExtensionCx, ExtensionEventLoop)> {
    let (loop_side, worker_side) = UnixStream::pair()?;
    loop_side.set_nonblocking(true)?;
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let events = ExtensionEvents {
        queue: queue.clone(),
        wake: Arc::new(Mutex::new(worker_side)),
    };
    Ok((
        ExtensionCx {
            events,
            next_request,
            streams,
            actions,
            apps_root,
        },
        ExtensionEventLoop {
            queue,
            wake: loop_side,
            active,
        },
    ))
}
