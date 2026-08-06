//! Fixed native operations exposed to the self-contained React bundle.

mod actions;
mod app_registry;
#[cfg(test)]
mod bundle_tests;
mod clipboard;
mod extension;
mod filesystem;
mod media;
#[cfg(test)]
mod media_constants_tests;
#[cfg(test)]
mod media_controls_tests;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use quickjs_runtime::{EngineError, NativeHost, Role};
use serde::{Deserialize, Serialize};

use crate::{
    audio::Commands as AudioCommands,
    tree::{self, Node},
};
use display_proto::Size;

pub use actions::Action;
pub use extension::{ExtensionCx, HostExtension};
// `ExtensionEvents` is the worker-thread event sink an extension hands to its
// download threads. Nothing inside the library uses it (the built-in ops don't
// spawn workers); it is public API consumed by app-binary extensions (Step 2+).
#[allow(unused_imports)]
pub use extension::ExtensionEvents;
pub(crate) use extension::{ExtensionEventLoop, extension_channel};

fn parse_u32(value: Option<&str>, name: &str) -> Result<u32, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

fn parse_u64(value: Option<&str>, name: &str) -> Result<u64, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

fn parse_i32(value: Option<&str>, name: &str) -> Result<i32, EngineError> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| EngineError::from_host(format!("invalid {name}")))
}

#[derive(Clone, Serialize)]
struct Bounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Serialize)]
struct Surface {
    id: u32,
    #[serde(rename = "appId")]
    app_id: String,
    title: String,
    icon: String,
    bounds: Bounds,
    #[serde(skip)]
    configure: Option<(u32, u32, u64)>,
}

/// Latest-only UI state and deferred native actions shared with the event loop.
pub struct State {
    scene: RefCell<Option<Vec<Node>>>,
    scene_dirty: Cell<bool>,
    composition_dirty: Cell<bool>,
    /// Deferred native side effects; shared with each `ExtensionCx` so a desktop
    /// extension can queue launch/shutdown/compositor actions.
    actions: extension::ActionQueue,
    surfaces: RefCell<Vec<Surface>>,
    next_configure: Cell<u64>,
    focused_surface: Cell<u32>,
    /// Move grab token from the compositor's last `MoveComplete`, echoed once in
    /// the next scene commit so the compositor retires the grab on token match.
    /// Set when the desktop processes `MoveComplete`, cleared after the commit
    /// that carries it — a one-shot the desktop never interprets, only relays.
    pending_move_token: Cell<u64>,
    timers: RefCell<Vec<(u64, Instant)>>,
    playback_granted: Cell<bool>,
    viewport: Cell<Size>,
    /// Generic streaming-download registry: maps a `stream:<id>` handle to its
    /// temp-file path and shared download state. An app extension (e.g. the
    /// music player) registers an entry (via [`ExtensionCx`]) when it starts a
    /// play-while-downloading fetch; `media.load` resolves it here so the
    /// decoder streams the growing file. Shared with each `ExtensionCx`; library
    /// infrastructure carrying no provider/source semantics.
    streams: extension::StreamRegistry,
    /// Run-loop side of the app-extension event channel. Extensions publish
    /// worker events through a shared queue and wake the loop over this fd; the
    /// loop drains them here. Inert when the process supplied no extensions.
    extensions: ExtensionEventLoop,
}

impl State {
    /// Reports whether the latest React snapshot still needs one raster pass.
    pub fn scene_is_dirty(&self) -> bool {
        self.scene_dirty.get()
    }

    /// Reports whether unchanged desktop pixels need a fresh foreign-surface latch.
    pub fn composition_is_dirty(&self) -> bool {
        self.composition_dirty.get()
    }

    /// Consumes one pending foreign-surface-only scene latch.
    pub fn take_composition_dirty(&self) -> bool {
        self.composition_dirty.replace(false)
    }

    /// Requests a new scene latch without invalidating desktop raster pixels.
    pub fn invalidate_composition(&self) {
        self.composition_dirty.set(true);
    }

    /// Borrows the latest React host snapshot when it still needs rendering.
    ///
    /// The scene stays owned so a reconfigure can re-render it at the new
    /// viewport without a per-frame clone: [`State::invalidate_scene`] simply
    /// marks the retained snapshot dirty again.
    pub fn scene_if_dirty(&self) -> Option<std::cell::Ref<'_, Vec<Node>>> {
        if !self.scene_dirty.replace(false) {
            return None;
        }
        std::cell::Ref::filter_map(self.scene.borrow(), Option::as_ref).ok()
    }

    /// Forces the retained snapshot to render once more (viewport change).
    pub fn invalidate_scene(&self) {
        self.scene_dirty.set(true);
    }

    /// Takes all native actions produced by the completed JavaScript turn.
    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    /// Takes every JavaScript timer whose deadline has passed.
    pub fn take_expired_timers(&self) -> Vec<u64> {
        let now = Instant::now();
        let mut timers = self.timers.borrow_mut();
        let expired = timers
            .iter()
            .filter(|(_, deadline)| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        timers.retain(|(_, deadline)| *deadline > now);
        expired
    }

    /// Returns how long the event loop may park until the next timer fires.
    pub fn next_timer_delay(&self) -> Option<Duration> {
        let now = Instant::now();
        self.timers
            .borrow()
            .iter()
            .map(|(_, deadline)| deadline.saturating_duration_since(now))
            .min()
    }

    /// Returns the desktop-selected focused app surface.
    pub fn focused_surface(&self) -> u32 {
        self.focused_surface.get()
    }

    /// Records the move grab token from a compositor `MoveComplete`, to be
    /// echoed once in the next scene commit.
    pub fn set_pending_move_token(&self, token: u64) {
        self.pending_move_token.set(token);
    }

    /// Takes the pending move grab token (one-shot: cleared to zero), so exactly
    /// one scene commit carries it back to the compositor.
    pub fn take_pending_move_token(&self) -> u64 {
        self.pending_move_token.replace(0)
    }

    /// Records one compositor-authenticated physical input activation.
    pub fn grant_media_playback(&self) {
        self.playback_granted.set(true);
    }

    /// Updates the browser-standard logical viewport exposed to JavaScript.
    pub fn set_viewport(&self, viewport: Size) {
        self.viewport.set(viewport);
    }

    /// The pollable wake fd for app-extension worker events, present only when
    /// this process supplied at least one extension.
    pub fn extension_event_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.extensions.as_fd()
    }

    /// Drains all `(channel, payload)` events published by extension workers
    /// since the last poll, for the run loop to dispatch to JavaScript.
    pub fn drain_extension_events(&self) -> Vec<(String, serde_json::Value)> {
        self.extensions.drain()
    }

    /// Resolves a registered stream handle to its temp path and shared state.
    pub(crate) fn resolve_stream(&self, id: u64) -> Option<(PathBuf, crate::audio::SharedStream)> {
        self.streams
            .borrow()
            .get(&id)
            .map(|(path, state)| (path.clone(), state.clone()))
    }

    /// Adds one compositor-published app surface to desktop policy state.
    ///
    /// Focus is *not* assigned here: JS `open` is the sole z-order/focus
    /// authority (it drives `focus()` on the `opened` event). This registry is
    /// pure existence + metadata, carrying no ordering or focus semantics.
    pub fn open_surface(&self, id: u32, app_id: String) {
        // Cascade off the count of currently-live surfaces, not a monotonic
        // counter. A monotonic index never decremented on close, so open/close
        // churn drifted the `% 4` slot and walked new windows off-screen.
        let index = self.surfaces.borrow().len() as u32;
        let (title, icon) = app_registry::app_metadata(&app_id);
        let bounds = match app_id.as_str() {
            "file-manager" => Bounds {
                x: 76,
                y: 124,
                width: 834,
                height: 540,
            },
            "terminal" => Bounds {
                x: 958,
                y: 414,
                width: 460,
                height: 250,
            },
            "music-player" | "my-computer" => {
                let slot = index % 4;
                Bounds {
                    x: 180 + slot * 28,
                    y: 58 + slot * 24,
                    width: 900,
                    height: 620,
                }
            }
            _ => Bounds {
                x: 260 + index * 28,
                y: 150 + index * 24,
                width: 720,
                height: 480,
            },
        };
        self.surfaces.borrow_mut().push(Surface {
            id,
            app_id,
            title: title.to_owned(),
            icon: icon.to_owned(),
            bounds,
            configure: None,
        });
    }

    /// Removes one disconnected app surface from desktop policy state.
    ///
    /// Focus fallback is *not* chosen here — JS `open` owns it. Closing the
    /// focused surface only clears the keyboard target to the desktop (0) so a
    /// key never routes to a dead surface; JS then assigns the next focus
    /// through its own z-order/workspace policy on the `closed` event.
    pub fn close_surface(&self, id: u32) {
        self.surfaces
            .borrow_mut()
            .retain(|surface| surface.id != id);
        if self.focused_surface.get() == id {
            self.focused_surface.set(0);
        }
    }

    pub(crate) fn move_surface(&self, id: u32, x: u32, y: u32) -> Result<(), EngineError> {
        let mut surfaces = self.surfaces.borrow_mut();
        // Guest-side mirror of the compositor's one-frame surface-set lag (see
        // `Session::app_is_live` in the compositor): the desktop's resize/move
        // commit can name a window whose app closed mid-drag, one React frame
        // before the `closed` event prunes it (native MoveComplete races the
        // same way). The desktop reconciles on the next AppClosed, so a missing
        // surface here is a recoverable race, not corruption — no-op instead of
        // throwing, which (uncaught in JS) would exit the whole desktop under
        // panic=abort.
        let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == id) else {
            eprintln!("lite-ui: move for surface {id} dropped (app disconnect race)");
            return Ok(());
        };
        surface.bounds.x = x;
        surface.bounds.y = y;
        Ok(())
    }
}

/// One JSON chord accepted by `desktop.accelerators.set`; it mirrors
/// [`display_proto::AcceleratorChord`] but is decodable from the JS payload.
#[derive(Deserialize)]
struct AcceleratorChordPayload {
    modifiers: u32,
    code: u32,
}

/// QuickJS native bridge implementation for one LiteUI process.
pub struct Host {
    role: Role,
    started: Instant,
    state: Rc<State>,
    app_root: PathBuf,
    files: RefCell<filesystem::Files>,
    audio: RefCell<AudioCommands>,
    /// App-provided native op handlers, tried after the built-in cascade.
    extensions: RefCell<Vec<Box<dyn HostExtension>>>,
    /// Generic context handed to each extension invocation (requestId alloc +
    /// worker event sink), carrying no app semantics.
    extension_cx: ExtensionCx,
    next_media: Cell<u64>,
    next_clipboard_request: Cell<u64>,
    media: RefCell<BTreeSet<u64>>,
}

impl Host {
    /// Creates the unique native host and its read-side state handle.
    ///
    /// # Parameters
    ///
    /// - `role`: Determines which native operations the JavaScript bundle may
    ///   invoke.
    /// - `app_root`: Resolves assets for the currently running desktop or app
    ///   bundle.
    /// - `apps_root`: The installed-application registry root, exposed to
    ///   extensions (the desktop launcher scans it) via [`ExtensionCx`].
    /// - `audio`: Sends media and system-volume commands to the audio service.
    /// - `extensions`: App-provided native op handlers (empty for apps that use
    ///   only the generic runtime capabilities).
    /// - `viewport`: Initial logical CSS viewport selected by the display.
    ///
    /// # Returns
    ///
    /// The native host installed into QuickJS and the retained state consumed
    /// by the display/event loop.
    pub fn new(
        role: Role,
        app_root: PathBuf,
        apps_root: PathBuf,
        audio: AudioCommands,
        extensions: Vec<Box<dyn HostExtension>>,
        viewport: Size,
    ) -> (Self, Rc<State>) {
        // The extension request-id counter is shared between the host (which
        // allocates ids inside `invoke`) and the context handed to extensions.
        let next_extension_request = Rc::new(Cell::new(1u64));
        // The stream registry is shared between `State` (which resolves
        // `stream:<id>` for `media.load`) and each `ExtensionCx` (which registers
        // downloads). One `Rc`, two owners on the same thread.
        let streams: extension::StreamRegistry = Rc::new(RefCell::new(Default::default()));
        // The deferred-action queue is shared between `State` (drained by the
        // run loop) and each `ExtensionCx` (a desktop extension pushes launch/
        // shutdown/compositor actions). One `Rc`, two owners on the same thread.
        let actions: extension::ActionQueue = Rc::new(RefCell::new(Vec::new()));
        let (extension_cx, extension_loop) = extension_channel(
            !extensions.is_empty(),
            next_extension_request,
            streams.clone(),
            actions.clone(),
            apps_root.clone(),
        )
        .expect("extension channel");
        let state = Rc::new(State {
            scene: RefCell::new(None),
            scene_dirty: Cell::new(false),
            composition_dirty: Cell::new(false),
            actions,
            surfaces: RefCell::new(Vec::new()),
            next_configure: Cell::new(1),
            focused_surface: Cell::new(0),
            pending_move_token: Cell::new(0),
            timers: RefCell::new(Vec::new()),
            playback_granted: Cell::new(false),
            viewport: Cell::new(viewport),
            streams,
            extensions: extension_loop,
        });
        (
            Self {
                role,
                started: Instant::now(),
                state: state.clone(),
                app_root,
                files: RefCell::new(filesystem::Files::default()),
                audio: RefCell::new(audio),
                extensions: RefCell::new(extensions),
                extension_cx,
                next_media: Cell::new(1),
                next_clipboard_request: Cell::new(1),
                media: RefCell::new(BTreeSet::new()),
            },
            state,
        )
    }

    fn desktop_configure(&self, payload: &str) -> Result<String, EngineError> {
        let mut fields = payload.split(':');
        let surface_id = parse_u32(fields.next(), "surface id")?;
        let width = parse_u32(fields.next(), "surface width")?;
        let height = parse_u32(fields.next(), "surface height")?;
        if fields.next().is_some() || width == 0 || height == 0 {
            return Err(EngineError::from_host("invalid desktop configure"));
        }
        let mut surfaces = self.state.surfaces.borrow_mut();
        // `configure` is called during JSX render for every surface node, so it
        // can name a surface that disconnected one frame ago (before `reconcile`
        // prunes it). Emitting no Configure action and returning a benign serial
        // lets that final render complete; the surface vanishes next frame. A
        // throw here is uncaught in JS and would exit the whole desktop under
        // panic=abort — the very crash a rapid resize was triggering.
        let Some(surface) = surfaces.iter_mut().find(|surface| surface.id == surface_id) else {
            eprintln!(
                "lite-ui: configure for unknown surface {surface_id} skipped (disconnect race)"
            );
            return Ok("0".to_string());
        };
        if let Some((old_width, old_height, serial)) = surface.configure
            && old_width == width
            && old_height == height
        {
            return Ok(serial.to_string());
        }
        let serial = self.state.next_configure.get();
        self.state.next_configure.set(
            serial
                .checked_add(1)
                .ok_or_else(|| EngineError::from_host("configure identity exhausted"))?,
        );
        surface.configure = Some((width, height, serial));
        self.state.actions.borrow_mut().push(Action::Configure {
            surface_id,
            serial,
            width,
            height,
        });
        Ok(serial.to_string())
    }

    /// Validates and queues one atomic accelerator-table replacement.
    ///
    /// The chord list is the desktop's complete shortcut set: overlong tables
    /// are rejected here so the deferred wire encode in
    /// [`display_proto::AcceleratorSet::encode`] can never fail on length.
    fn desktop_accelerators(&self, payload: &str) -> Result<String, EngineError> {
        let chords: Vec<AcceleratorChordPayload> = serde_json::from_str(payload)
            .map_err(|error| EngineError::from_host(error.to_string()))?;
        if chords.len() > display_proto::MAX_ACCELERATORS {
            return Err(EngineError::from_host("accelerator table exceeds limit"));
        }
        self.state
            .actions
            .borrow_mut()
            .push(Action::SetAccelerators(
                chords
                    .into_iter()
                    .map(|chord| display_proto::AcceleratorChord {
                        modifiers: chord.modifiers,
                        code: chord.code,
                    })
                    .collect(),
            ));
        Ok(String::new())
    }
}

impl NativeHost for Host {
    fn invoke(&mut self, operation: &str, payload: &str) -> Result<String, EngineError> {
        if let Some(result) = self.invoke_media(operation, payload) {
            return result;
        }
        // App-provided native ops (music provider/network, desktop shell,
        // terminal PTY, ...) layer on after the built-in cascade. First
        // extension to claim the op wins; `None` = not mine, keep looking.
        {
            let mut extensions = self.extensions.borrow_mut();
            for extension in extensions.iter_mut() {
                if let Some(result) = extension.invoke(&self.extension_cx, operation, payload) {
                    return result;
                }
            }
        }
        match operation {
            "scene.commit" => {
                let scene = tree::parse(payload).map_err(EngineError::from_host)?;
                self.state.scene.replace(Some(scene));
                self.state.scene_dirty.set(true);
                self.state.composition_dirty.set(false);
                Ok(String::new())
            }
            "time.now" => Ok(self
                .started
                .elapsed()
                .as_secs_f64()
                .mul_add(1000.0, 0.0)
                .to_string()),
            "viewport.get" => {
                let viewport = self.state.viewport.get();
                Ok(format!(
                    r#"{{"width":{},"height":{},"devicePixelRatio":{}}}"#,
                    viewport.width,
                    viewport.height,
                    display_proto::DEVICE_SCALE_FACTOR
                ))
            }
            // 1. Wall-clock seconds for desktop chrome (the tray clock); the
            //    monotonic `time.now` above stays for animation timing.
            "time.clock" => Ok(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| EngineError::from_host(error.to_string()))?
                .as_secs()
                .to_string()),
            "timer.set" => {
                let mut fields = payload.split(':');
                let id = parse_u64(fields.next(), "timer id")?;
                let delay = parse_u64(fields.next(), "timer delay")?;
                if fields.next().is_some() {
                    return Err(EngineError::from_host("invalid timer set"));
                }
                self.state
                    .timers
                    .borrow_mut()
                    .push((id, Instant::now() + Duration::from_millis(delay)));
                Ok(String::new())
            }
            "timer.clear" => {
                let id = parse_u64(Some(payload), "timer id")?;
                self.state
                    .timers
                    .borrow_mut()
                    .retain(|(timer, _)| *timer != id);
                Ok(String::new())
            }
            "desktop.surfaces" if self.role == Role::Desktop => serde_json::to_string(&*self.state.surfaces.borrow()).map_err(|error| EngineError::from_host(error.to_string())),
            "desktop.configure" if self.role == Role::Desktop => self.desktop_configure(payload),
            "desktop.accelerators.set" if self.role == Role::Desktop => {
                self.desktop_accelerators(payload)
            }
            "desktop.focus" if self.role == Role::Desktop => {
                let surface_id = parse_u32(Some(payload), "focused surface")?;
                self.state.focused_surface.set(surface_id);
                Ok(String::new())
            }
            "desktop.move" if self.role == Role::Desktop => {
                let mut fields = payload.split(':');
                let surface_id = parse_u32(fields.next(), "moved surface")?;
                let x = parse_u32(fields.next(), "surface x")?;
                let y = parse_u32(fields.next(), "surface y")?;
                if fields.next().is_some() {
                    return Err(EngineError::from_host("invalid desktop move"));
                }
                self.state.move_surface(surface_id, x, y)?;
                Ok(String::new())
            }
            "desktop.move.begin" if self.role == Role::Desktop => {
                let mut fields = payload.split(':');
                let surface_id = parse_u32(fields.next(), "moved surface")?;
                let serial = parse_u64(fields.next(), "pointer serial")?;
                let min_x = parse_i32(fields.next(), "minimum x")?;
                let min_y = parse_i32(fields.next(), "minimum y")?;
                let max_x = parse_i32(fields.next(), "maximum x")?;
                let max_y = parse_i32(fields.next(), "maximum y")?;
                if fields.next().is_some() || max_x < min_x || max_y < min_y {
                    return Err(EngineError::from_host("invalid desktop move bounds"));
                }
                self.state.actions.borrow_mut().push(Action::BeginMove {
                    surface_id,
                    serial,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                });
                Ok(String::new())
            }
            "desktop.close" if self.role == Role::Desktop => {
                self.state.actions.borrow_mut().push(Action::Close(parse_u32(Some(payload), "closed surface")?));
                Ok(String::new())
            }
            // The pre-first-update frame mirrors the helper's initial palette
            // (terminal-session DEFAULT_COLORS indices 7 and 0); the first real
            // screen update replaces both colors and rows.
            "terminal.connect" if self.role == Role::App => Ok(
                r#"{"rows":[[{"text":"Connecting to LiteOS terminal...","fg":13358561,"bg":1053720,"bold":false}]],"cursor":{"column":0,"row":0},"foreground":13358561,"background":1053720}"#.to_owned(),
            ),
            "terminal.input" if self.role == Role::App => {
                self.state.actions.borrow_mut().push(Action::TerminalInput(payload.as_bytes().to_vec()));
                Ok(String::new())
            }
            "terminal.paste" if self.role == Role::App => self.terminal_paste(payload),
            "clipboard.read" => self.clipboard_read(),
            "clipboard.write" => self.clipboard_write(payload),
            "fs.list" if self.role == Role::App => Ok(filesystem::list(payload)),
            "fs.read" if self.role == Role::App => Ok(filesystem::read(payload)),
            "fs.mkdir" if self.role == Role::App => Ok(filesystem::mkdir(payload)),
            "fs.remove" if self.role == Role::App => Ok(filesystem::remove(payload)),
            "fs.rename" if self.role == Role::App => Ok(filesystem::rename(payload)),
            "fs.copy" if self.role == Role::App => Ok(filesystem::copy(payload)),
            _ => Err(EngineError::from_host(format!(
                "operation '{operation}' is unavailable in this session"
            ))),
        }
    }
}
