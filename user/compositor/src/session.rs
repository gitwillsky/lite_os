//! Strict multi-process display session and compositor-owned client buffers.

mod buffers;
mod clipboard_bridge;
mod client;
mod cursor;
mod messages;
mod routing;
mod scene;
mod wire;

pub use buffers::Buffers;
use buffers::Owner;
use cursor::{cursor_on_focus_change, cursor_request};
pub use scene::{Node, Scene};
use wire::{new_epoch, send_accepted, send_presented};

use std::{
    collections::HashMap,
    fs, io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

use display_proto::{
    AppClosed, AppOpened, CloseRequest, Configure, ConfigureReady, MAX_APP_SURFACES, Rect,
    SetCursorShape, Size, SurfaceCommit, send_message,
};
use linux_uapi::{
    drm::DrmDevice,
    unix::{self, PollEvents, PollFd},
};

/// One presented surface's hit-test region, retained for input routing.
#[derive(Clone)]
struct RoutingNode {
    surface_id: u32,
    window_group: u32,
    bounds: Rect,
    input: Vec<Rect>,
}

#[derive(Clone, Copy)]
struct PointerCapture {
    surface_id: u32,
    window_group: u32,
    bounds: Rect,
    serial: u64,
    down: (i32, i32),
}

#[derive(Clone, Copy)]
struct MoveGrab {
    surface_id: u32,
    underlay_buffer_id: u32,
    down: (i32, i32),
    origin: (i32, i32),
    offset: (i32, i32),
    limits: (i32, i32, i32, i32),
    ending: bool,
}

struct Desktop {
    stream: UnixStream,
    last_revision: u64,
}

#[derive(Clone)]
struct Content {
    revision: u64,
    configure_serial: u64,
    buffer_id: u32,
    damage: Vec<Rect>,
}

struct App {
    stream: UnixStream,
    id: String,
    configure: Option<Configure>,
    last_revision: u64,
    pending: Option<Content>,
    current: Option<Content>,
    // Prevents automation from clicking before this window enters input routing.
    first_scene_presented: bool,
}

/// One compositor epoch. Desktop disconnect clears every app and client buffer.
pub struct Session {
    listener: UnixListener,
    device: DrmDevice,
    display: Size,
    epoch: u64,
    desktop: Option<Desktop>,
    apps: HashMap<u32, App>,
    buffers: Buffers,
    next_buffer_id: u32,
    next_surface_id: u32,
    first_scene_presented: bool,
    routing: Vec<RoutingNode>,
    focused_surface: u32,
    pointer_capture: Option<PointerCapture>,
    move_grab: Option<MoveGrab>,
    move_damage: Option<Rect>,
    presented_nodes: Vec<scene::Node>,
    desktop_current_buffers: Vec<u32>,
    last_flip: linux_uapi::drm::FlipEvent,
    /// Cursor shape requested by a client since the last poll, drained into
    /// [`Activity`] so the caller (which owns scanout and the cursor) applies it.
    pending_cursor_shape: Option<u32>,
    /// Surface currently under the pointer (the pointer-focus target); `None`
    /// when no surface is hit. Cursor requests are honored only from this
    /// surface, and a focus change resets the cursor to the default arrow so a
    /// prior surface's shape never leaks onto the next one.
    pointer_surface: Option<u32>,
    clipboard: crate::clipboard::Clipboard,
}

/// Outcome of one [`Session::poll`] wait.
pub struct Activity {
    /// A newly accepted desktop scene ready to compose, if any.
    pub scene: Option<Scene>,
    /// Whether a caller-supplied wake descriptor (evdev) became readable.
    pub input: bool,
    /// Whether this poll reset the session epoch (desktop disconnect). The
    /// caller owns scanout state, which `reset_epoch` cannot reach, so it must
    /// return scanout to boot to avoid painting a stale scene diff on restart.
    pub epoch_reset: bool,
}

impl Session {
    /// Creates the only display socket and starts an empty epoch.
    pub fn open(device: &DrmDevice, display: Size) -> io::Result<Self> {
        let _ = fs::remove_file(display_proto::SOCKET_PATH);
        let listener = UnixListener::bind(display_proto::SOCKET_PATH)?;
        listener.set_nonblocking(true)?;
        let clipboard = crate::clipboard::Clipboard::open()?;
        Ok(Self {
            listener,
            device: device.clone(),
            display,
            epoch: new_epoch(),
            desktop: None,
            apps: HashMap::new(),
            buffers: Buffers {
                values: HashMap::new(),
            },
            next_buffer_id: 1,
            next_surface_id: 1,
            first_scene_presented: false,
            routing: Vec::new(),
            focused_surface: 0,
            pointer_capture: None,
            move_grab: None,
            move_damage: None,
            presented_nodes: Vec::new(),
            desktop_current_buffers: Vec::new(),
            last_flip: linux_uapi::drm::FlipEvent {
                user_data: 0,
                seconds: 0,
                microseconds: 0,
                sequence: 0,
            },
            pending_cursor_shape: None,
            pointer_surface: None,
            clipboard,
        })
    }

    /// Returns immutable compositor-owned client buffers used for composition.
    pub fn buffers(&self) -> &Buffers {
        &self.buffers
    }

    /// Reports whether a desktop scene has reached flip completion.
    pub fn desktop_ready(&self) -> bool {
        self.first_scene_presented
    }

    /// Polls all display connections plus caller-supplied wake descriptors once.
    ///
    /// The `wake` descriptors (evdev fds) join the same wait so pointer and key
    /// events interrupt the timeout immediately instead of waiting for it to
    /// elapse. Without them the loop would only drain input once per timeout,
    /// capping cursor updates at roughly `1 / timeout` Hz and adding up to one
    /// timeout of latency per move. Their readiness is returned in [`Activity`]
    /// so the caller can pump input, while at most one accepted scene is returned.
    pub fn poll(
        &mut self,
        wake: &[Option<BorrowedFd<'_>>],
        timeout: Duration,
    ) -> io::Result<Activity> {
        let mut app_ids = [0; MAX_APP_SURFACES];
        let mut app_count = 0;
        for id in self.apps.keys().copied() {
            app_ids[app_count] = id;
            app_count += 1;
        }
        let (listener_ready, desktop_ready, app_ready, input_ready, clipboard_ready) = {
            const MAX_POLL_FDS: usize = 3 + MAX_APP_SURFACES + 2;
            let mut descriptors: [PollFd; MAX_POLL_FDS] =
                std::array::from_fn(|_| PollFd::new(self.listener.as_fd(), PollEvents::READ));
            let mut descriptor_count = 0;
            descriptors[descriptor_count] = PollFd::new(self.listener.as_fd(), PollEvents::READ);
            descriptor_count += 1;
            if let Some(desktop) = &self.desktop {
                descriptors[descriptor_count] =
                    PollFd::new(desktop.stream.as_fd(), PollEvents::READ);
                descriptor_count += 1;
            }
            for id in &app_ids[..app_count] {
                descriptors[descriptor_count] =
                    PollFd::new(self.apps[id].stream.as_fd(), PollEvents::READ);
                descriptor_count += 1;
            }
            let wake_offset = descriptor_count;
            for fd in wake.iter().flatten() {
                descriptors[descriptor_count] = PollFd::new(*fd, PollEvents::READ);
                descriptor_count += 1;
            }
            let clipboard_offset =
                self.append_clipboard_poll(&mut descriptors, &mut descriptor_count);
            unix::poll(&mut descriptors[..descriptor_count], Some(timeout))?;
            let listener_ready = descriptors[0].returned().contains(PollEvents::READ);
            let desktop_offset = usize::from(self.desktop.is_some());
            let desktop_ready =
                self.desktop.is_some() && descriptors[1].returned() != PollEvents::EMPTY;
            let mut app_ready = [false; MAX_APP_SURFACES];
            for (ready, descriptor) in app_ready[..app_count]
                .iter_mut()
                .zip(&descriptors[1 + desktop_offset..wake_offset])
            {
                *ready = descriptor.returned() != PollEvents::EMPTY;
            }
            let input_ready = descriptors[wake_offset..clipboard_offset]
                .iter()
                .any(|descriptor| descriptor.returned() != PollEvents::EMPTY);
            let clipboard_ready = descriptors[clipboard_offset].returned() != PollEvents::EMPTY;
            (
                listener_ready,
                desktop_ready,
                app_ready,
                input_ready,
                clipboard_ready,
            )
        };
        if listener_ready && let Err(error) = self.accept() {
            eprintln!("compositor: rejected connection: {error}");
        }
        let mut scene = None;
        if desktop_ready {
            match self.receive_desktop() {
                Ok(accepted) if accepted.is_some() => scene = accepted,
                Ok(_) => {}
                Err(error) => {
                    eprintln!("compositor: desktop disconnected: {error}");
                    self.reset_epoch();
                    self.pending_cursor_shape = None;
                    self.pointer_surface = None;
                    return Ok(Activity {
                        scene: None,
                        input: input_ready,
                        epoch_reset: true,
                    });
                }
            }
        }
        for (surface_id, ready) in app_ids[..app_count]
            .iter()
            .copied()
            .zip(app_ready[..app_count].iter().copied())
        {
            if ready && let Err(error) = self.receive_app(surface_id) {
                eprintln!("compositor: app {surface_id} disconnected: {error}");
                self.remove_app(surface_id);
            }
        }
        if clipboard_ready {
            self.pump_clipboard()?;
        }
        Ok(Activity {
            scene,
            input: input_ready,
            epoch_reset: false,
        })
    }

    /// Drains the pending cursor shape, if any. The caller (which owns scanout)
    /// applies it after routing this iteration's pointer motion, so the shape
    /// reflects the final pointer-focus surface rather than a stale mid-batch one.
    pub fn take_cursor_shape(&mut self) -> Option<u32> {
        self.pending_cursor_shape.take()
    }

    /// Updates the pointer-focus surface. On a real change it resets the cursor
    /// to the default arrow: the surface being left cannot send a reset (input
    /// no longer routes to it), and the surface being entered owns its cursor
    /// only from its next Motion. Without this reset a resize shape set over a
    /// window edge would persist after the pointer moved into the content
    /// surface (or off it), since no party would ever request the arrow back.
    fn set_pointer_surface(&mut self, next: Option<u32>) {
        if let Some(shape) = cursor_on_focus_change(self.pointer_surface, next) {
            self.pending_cursor_shape = Some(shape);
        }
        self.pointer_surface = next;
    }

    /// Accepts a `SetCursorShape` request from `source_surface` (the connection
    /// it arrived on; zero for the desktop). Rejects a spoofed surface id, and
    /// applies the shape only while `source_surface` still holds pointer focus
    /// so an async request that arrives after focus moved away is ignored.
    fn accept_cursor_shape(
        &mut self,
        source_surface: u32,
        request: SetCursorShape,
    ) -> io::Result<()> {
        match cursor_request(self.pointer_surface, source_surface, &request) {
            Err(()) => Err(invalid("cursor shape surface does not match connection")),
            Ok(Some(shape)) => {
                self.pending_cursor_shape = Some(shape);
                Ok(())
            }
            Ok(None) => Ok(()),
        }
    }

    fn route_configure(&mut self, configure: Configure) -> io::Result<()> {
        // The desktop bakes a Configure from React state and can legitimately
        // target an app the compositor already removed: an app disconnect races
        // the desktop's next commit (the compositor drops the app before
        // AppClosed reaches the desktop). That is the same recoverable race the
        // scene path skips for foreign surfaces — swallow it here instead of
        // tearing down the whole desktop epoch. Every other violation
        // (non-monotonic serial, encoding failure, socket error) stays fatal.
        let Some(app) = self.apps.get_mut(&configure.surface_id) else {
            eprintln!(
                "compositor: configure for unknown app {} dropped (disconnect race)",
                configure.surface_id
            );
            return Ok(());
        };
        if app
            .configure
            .is_some_and(|current| configure.serial <= current.serial)
        {
            return Err(invalid("configure serial is not monotonic"));
        }
        let mut bytes = [0u8; 40];
        let message = configure
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("configure encoding failed"))?;
        send_message(&app.stream, message)?;
        app.configure = Some(configure);
        Ok(())
    }

    fn route_close(&self, surface_id: u32) -> io::Result<()> {
        // Same disconnect race as `route_configure`: the desktop may close a
        // window whose app already vanished. Nothing left to forward the request
        // to, and the desktop will reconcile on the next AppClosed — a no-op, not
        // a fatal protocol error.
        let Some(app) = self.apps.get(&surface_id) else {
            eprintln!("compositor: close for unknown app {surface_id} dropped (disconnect race)");
            return Ok(());
        };
        let mut bytes = [0u8; 24];
        let message = CloseRequest { surface_id }
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("close encoding failed"))?;
        send_message(&app.stream, message)
    }

    fn accept_surface(&mut self, surface_id: u32, commit: SurfaceCommit<'_>) -> io::Result<()> {
        let app = self
            .apps
            .get(&surface_id)
            .ok_or_else(|| invalid("unknown app"))?;
        let Some(configure) = app.configure else {
            return Err(invalid("surface commit configure missing"));
        };
        let buffer = self
            .buffers
            .values
            .get(&commit.buffer_id)
            .ok_or_else(|| invalid("unknown app buffer"))?;
        if commit.revision <= app.last_revision
            || buffer.owner != Owner::App(surface_id)
            || buffer.busy
        {
            return Err(invalid("surface commit state invalid"));
        }
        if configure.serial != commit.configure_serial {
            // Resize race: a newer configure superseded this frame before it
            // arrived. ACK and present it immediately so the app's frame
            // pacing unblocks, and recycle the never-adopted buffer; the next
            // commit carries the current configure's geometry.
            let app = self.apps.get_mut(&surface_id).expect("validated app");
            app.last_revision = commit.revision;
            send_accepted(&app.stream, commit.revision)?;
            return send_presented(&app.stream, commit.revision, self.last_flip);
        }
        if buffer.size.width != configure.width * display_proto::DEVICE_SCALE_FACTOR
            || buffer.size.height != configure.height * display_proto::DEVICE_SCALE_FACTOR
        {
            return Err(invalid("surface commit state invalid"));
        }
        let buffer_size = buffer.size;
        // A pending frame that the desktop has not yet adopted into a scene is
        // superseded by this newer commit at the same serial. Maximize/restore
        // makes an app reconfigure and repaint faster than the desktop adopts
        // its frames, so back-to-back app commits are normal, not a violation:
        // recycle the never-presented pending buffer and let the new frame take
        // its slot. (`pending` is cleared only by desktop adoption, so a present
        // `pending` provably never reached the screen and is safe to release.)
        if let Some(superseded) = app.pending.as_ref() {
            scene::release_buffer(
                &mut self.buffers,
                &self.apps[&surface_id].stream,
                superseded.buffer_id,
            )?;
            self.apps
                .get_mut(&surface_id)
                .expect("validated app")
                .pending = None;
        }
        let content = Content {
            revision: commit.revision,
            configure_serial: commit.configure_serial,
            buffer_id: commit.buffer_id,
            damage: {
                let damage: Vec<_> = commit.damage().collect();
                if damage.is_empty() {
                    vec![Rect {
                        x: 0,
                        y: 0,
                        width: buffer_size.width,
                        height: buffer_size.height,
                    }]
                } else {
                    if damage.iter().any(|rectangle| {
                        rectangle.x < 0
                            || rectangle.y < 0
                            || rectangle.x.saturating_add_unsigned(rectangle.width)
                                > buffer_size.width as i32
                            || rectangle.y.saturating_add_unsigned(rectangle.height)
                                > buffer_size.height as i32
                    }) {
                        return Err(invalid("surface damage outside buffer"));
                    }
                    damage
                }
            },
        };
        self.buffers
            .values
            .get_mut(&commit.buffer_id)
            .expect("validated app buffer")
            .busy = true;
        let app = self.apps.get_mut(&surface_id).expect("validated app");
        app.last_revision = commit.revision;
        app.pending = Some(content);
        send_accepted(&app.stream, commit.revision)?;
        let desktop = self.desktop_stream()?;
        let mut bytes = [0u8; 32];
        let message = ConfigureReady {
            surface_id,
            serial: commit.configure_serial,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("ready encoding failed"))?;
        send_message(desktop, message)
    }

    fn notify_opened(&self, surface_id: u32) -> io::Result<()> {
        let app = &self.apps[&surface_id];
        let mut bytes = [0u8; 128];
        let message = AppOpened {
            surface_id,
            app_id: app.id.as_bytes(),
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("opened encoding failed"))?;
        send_message(self.desktop_stream()?, message)
    }

    fn remove_app(&mut self, surface_id: u32) {
        if self.apps.remove(&surface_id).is_none() {
            return;
        }
        self.clipboard.remove_surface(surface_id);
        self.buffers
            .values
            .retain(|_, buffer| buffer.owner != Owner::App(surface_id));
        self.clear_pointer_capture(Some(surface_id));
        if let Ok(stream) = self.desktop_stream() {
            let mut bytes = [0u8; 24];
            if let Some(message) = (AppClosed { surface_id }).encode(&mut bytes) {
                let _ = send_message(stream, message);
            }
        }
    }

    fn desktop_stream(&self) -> io::Result<&UnixStream> {
        self.desktop
            .as_ref()
            .map(|desktop| &desktop.stream)
            .ok_or_else(|| io::Error::other("desktop is not connected"))
    }

    fn take_surface_id(&mut self) -> io::Result<u32> {
        let id = self.next_surface_id;
        self.next_surface_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("surface identity exhausted"))?;
        Ok(id)
    }

    fn reset_epoch(&mut self) {
        self.desktop = None;
        self.apps.clear();
        self.buffers.values.clear();
        self.first_scene_presented = false;
        self.routing.clear();
        self.focused_surface = 0;
        self.clear_pointer_capture(None);
        self.move_grab = None;
        self.move_damage = None;
        self.presented_nodes.clear();
        self.desktop_current_buffers.clear();
        self.clipboard.reset_session();
        self.epoch = self.epoch.wrapping_add(1);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = fs::remove_file(display_proto::SOCKET_PATH);
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests;
