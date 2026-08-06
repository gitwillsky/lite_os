//! Strict multi-process display session and compositor-owned client buffers.

mod accelerator;
mod buffers;
mod client;
mod clipboard_bridge;
mod cursor;
mod messages;
mod output;
mod paint_store;
mod routing;
mod scene;
mod wire;

pub use buffers::Buffers;
pub(crate) use buffers::{Owner, PaintTarget};
use cursor::{cursor_on_focus_change, cursor_request};
use paint_store::PaintStore;
pub use scene::{Node, Scene};
use wire::{new_epoch, send_accepted};

use accelerator::Accelerators;

use std::{
    collections::{HashMap, VecDeque},
    fs, io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::{UnixListener, UnixStream},
};

use display_proto::{
    AppClosed, AppOpened, CloseRequest, Configure, ConfigureReady, DisplayListCommit,
    MAX_APP_SURFACES, MoveBegin, Rect, SetCursorShape, Size, TextureFormat, send_message,
};
use linux_uapi::{
    drm::{DrmDevice, VirglContext, VirglResource},
    kobject::KobjectUevent,
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
    graphics: VirglContext,
    display: Size,
    output_serial: u64,
    hotplug: KobjectUevent,
    epoch: u64,
    desktop: Option<Desktop>,
    apps: HashMap<u32, App>,
    buffers: Buffers,
    paint: PaintStore,
    pending_paint: VecDeque<Owner>,
    desktop_render_id: Option<u32>,
    /// One retired GPU paint target and its accumulated stale region per client
    /// owner. The exact owner key prevents cross-surface overwrite; retaining
    /// the repair region prevents retained scroll/resize from copying the full
    /// output merely because double buffering alternates target storage.
    idle_targets: HashMap<Owner, buffers::PaintTarget>,
    next_buffer_id: u32,
    next_surface_id: u32,
    first_scene_presented: bool,
    routing: Vec<RoutingNode>,
    focused_surface: u32,
    pointer_capture: Option<PointerCapture>,
    move_grab: Option<MoveGrab>,
    // Records that the canonical move offset changed since input last asked
    // scanout to draw. Without it, clamped pointer motion submits duplicate flips.
    move_changed: bool,
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
    /// Global accelerator chords owned by the desktop connection (atomic
    /// `AcceleratorSet` replacement) plus the active key grab. Cleared on
    /// epoch reset; without it every key routes only to the focused surface
    /// and system shortcuts cannot work.
    accelerators: Accelerators,
    spice_agent: crate::spice_agent::SpiceAgent,
}

/// Outcome of one [`Session::poll`] wait.
pub struct Activity {
    /// A newly accepted desktop scene ready to compose, if any.
    pub scene: Option<Scene>,
    /// Client display lists waiting for compositor-owned GPU rasterization.
    pub(crate) paint: Vec<Owner>,
    /// One desktop-authorized move whose target underlay must be rendered now.
    pub(crate) move_begin: Option<MoveBegin>,
    /// Whether a caller-supplied wake descriptor (evdev) became readable.
    pub input: bool,
    /// Whether the compositor's asynchronous window-move page flip completed.
    pub flip: bool,
    /// Whether this poll reset the session epoch (desktop disconnect). The
    /// caller owns scanout state, which `reset_epoch` cannot reach, so it must
    /// return scanout to boot to avoid painting a stale scene diff on restart.
    pub epoch_reset: bool,
    /// Latest physical connector size selected by a drained DRM hotplug burst.
    pub output: Option<Size>,
}

impl Session {
    /// Creates the only display socket and starts an empty epoch.
    pub fn open(device: &DrmDevice, graphics: &VirglContext, display: Size) -> io::Result<Self> {
        let _ = fs::remove_file(display_proto::SOCKET_PATH);
        let listener = UnixListener::bind(display_proto::SOCKET_PATH)?;
        listener.set_nonblocking(true)?;
        let spice_agent = crate::spice_agent::SpiceAgent::open()?;
        let hotplug = KobjectUevent::open()?;
        Ok(Self {
            listener,
            device: device.clone(),
            graphics: graphics.clone(),
            display,
            output_serial: 1,
            hotplug,
            epoch: new_epoch(),
            desktop: None,
            apps: HashMap::new(),
            buffers: Buffers {
                values: HashMap::new(),
            },
            paint: PaintStore::new(),
            pending_paint: VecDeque::new(),
            desktop_render_id: None,
            idle_targets: HashMap::new(),
            next_buffer_id: 1,
            next_surface_id: 1,
            first_scene_presented: false,
            routing: Vec::new(),
            focused_surface: 0,
            pointer_capture: None,
            move_grab: None,
            move_changed: false,
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
            accelerators: Accelerators::new(),
            spice_agent,
        })
    }

    /// Returns immutable compositor-owned client buffers used for composition.
    pub fn buffers(&self) -> &Buffers {
        &self.buffers
    }

    pub(crate) fn queue_paint(&mut self, owner: Owner) {
        if !self.pending_paint.contains(&owner) {
            self.pending_paint.push_back(owner);
        }
    }

    pub(crate) fn paint_size(&self, owner: Owner) -> io::Result<Size> {
        let list = self
            .paint
            .list(owner)
            .ok_or_else(|| invalid("display list disappeared"))?;
        match owner {
            Owner::Desktop if list.configuration_serial == self.output_serial => Ok(self.display),
            Owner::App(surface_id) => {
                let configure = self
                    .apps
                    .get(&surface_id)
                    .and_then(|app| app.configure)
                    .ok_or_else(|| invalid("app display list has no configure"))?;
                if list.configuration_serial != configure.serial {
                    return Err(invalid("app display list configure is stale"));
                }
                Ok(Size {
                    width: configure.width * display_proto::DEVICE_SCALE_FACTOR,
                    height: configure.height * display_proto::DEVICE_SCALE_FACTOR,
                })
            }
            Owner::Desktop => Err(invalid("desktop display list output is stale")),
        }
    }

    pub(crate) fn paint_list(&self, owner: Owner) -> io::Result<DisplayListCommit<'_>> {
        self.paint
            .list(owner)
            .ok_or_else(|| invalid("display list disappeared"))
    }

    /// Returns the epoch-qualified identity for one client's GPU effect cache.
    pub(crate) fn paint_cache_owner(&self, owner: Owner) -> (u64, u32) {
        (
            self.epoch,
            match owner {
                Owner::Desktop => 0,
                Owner::App(surface_id) => surface_id,
            },
        )
    }

    pub(crate) fn paint_texture(
        &self,
        owner: Owner,
        texture_id: u32,
    ) -> Option<(&VirglResource, TextureFormat)> {
        self.paint.texture(owner, texture_id)
    }

    /// Takes the owner's retired half of its GPU paint double buffer.
    pub(crate) fn take_paint_target(
        &mut self,
        owner: Owner,
        size: Size,
    ) -> Option<buffers::PaintTarget> {
        let target = self.idle_targets.remove(&owner)?;
        (target.pixels.width() == size.width && target.pixels.height() == size.height)
            .then_some(target)
    }

    /// Resolves the exact pixel owner named by a retained display-list commit.
    pub(crate) fn paint_base(&self, owner: Owner, revision: u64) -> Option<&VirglResource> {
        if revision == 0 {
            return None;
        }
        let id = match owner {
            Owner::Desktop => self.desktop_render_id,
            Owner::App(surface_id) => self.apps.get(&surface_id).and_then(|app| {
                app.pending
                    .as_ref()
                    .filter(|content| content.revision == revision)
                    .or_else(|| {
                        app.current
                            .as_ref()
                            .filter(|content| content.revision == revision)
                    })
                    .map(|content| content.buffer_id)
            }),
        }?;
        self.buffers
            .values
            .get(&id)
            .filter(|buffer| buffer.revision == revision)
            .map(|buffer| &buffer.pixels)
    }

    pub(super) fn recycle_buffer(&mut self, id: u32) {
        let Some(buffer) = self.buffers.values.remove(&id) else {
            return;
        };
        self.idle_targets.insert(
            buffer.owner,
            buffers::PaintTarget {
                pixels: buffer.pixels,
                repair: buffer.repair,
            },
        );
    }

    /// Publishes one submitted paint into the ordered display protocol.
    ///
    /// Paint and later scene composition use the compositor's sole VirGL
    /// context, so submission order is the synchronization contract. Waiting
    /// for the paint fence here would serialize those two GPU stages and add a
    /// complete fence latency to every scroll or keypress.
    pub(crate) fn publish_paint(
        &mut self,
        owner: Owner,
        target: VirglResource,
        revision: u64,
        configuration_serial: u64,
        damage: Rect,
    ) -> io::Result<()> {
        let size = Size {
            width: target.width(),
            height: target.height(),
        };
        // Every older paint target for this owner now differs from the new
        // canonical pixels exactly where this commit changed them. Accumulating
        // that region at publication keeps target/base ownership atomic; trying
        // to infer it later from revision numbers would lose non-consecutive or
        // empty-damage commits.
        for buffer in self
            .buffers
            .values
            .values_mut()
            .filter(|buffer| buffer.owner == owner && buffer.revision != 0)
        {
            buffer.repair = buffers::extend_repair(buffer.repair, damage);
        }
        let id = self.take_buffer_id()?;
        self.buffers.values.insert(
            id,
            buffers::Buffer {
                pixels: target,
                size,
                owner,
                busy: false,
                revision,
                repair: None,
            },
        );
        match owner {
            Owner::Desktop => {
                if let Some(previous) = self.desktop_render_id.replace(id)
                    && !self.desktop_current_buffers.contains(&previous)
                {
                    self.recycle_buffer(previous);
                }
                send_accepted(self.desktop_stream()?, revision)
            }
            Owner::App(surface_id) => {
                if let Some(previous) = self
                    .apps
                    .get(&surface_id)
                    .and_then(|app| app.pending.as_ref())
                    .map(|content| content.buffer_id)
                {
                    self.recycle_buffer(previous);
                }
                let app = self
                    .apps
                    .get_mut(&surface_id)
                    .ok_or_else(|| invalid("app disappeared during GPU paint"))?;
                app.last_revision = revision;
                app.pending = Some(Content {
                    revision,
                    configure_serial: configuration_serial,
                    buffer_id: id,
                });
                send_accepted(&app.stream, revision)?;
                let mut bytes = [0u8; 32];
                let ready = ConfigureReady {
                    surface_id,
                    serial: configuration_serial,
                }
                .encode(&mut bytes)
                .ok_or_else(|| io::Error::other("ready encoding failed"))?;
                send_message(self.desktop_stream()?, ready)
            }
        }
    }

    /// Reports whether a desktop scene has reached flip completion.
    pub fn desktop_ready(&self) -> bool {
        self.first_scene_presented
    }

    /// Polls all display connections plus caller-supplied wake descriptors once.
    ///
    /// The `wake` descriptors (evdev fds) join the same wait so pointer and key
    /// events wake the otherwise unbounded idle wait. Their readiness is
    /// returned in [`Activity`] so the caller can pump input, while at most one
    /// accepted scene is returned.
    ///
    /// # Parameters
    ///
    /// - `wake`: Optional evdev descriptors joined to the display/SPICE-agent wait.
    ///
    /// # Returns
    ///
    /// Returns the accepted scene and readiness/reset flags for this wake.
    ///
    /// # Errors
    ///
    /// Returns an error when polling or a required session transition fails.
    pub fn poll(&mut self, wake: &[Option<BorrowedFd<'_>>]) -> io::Result<Activity> {
        let mut app_ids = [0; MAX_APP_SURFACES];
        let mut app_count = 0;
        for id in self.apps.keys().copied() {
            app_ids[app_count] = id;
            app_count += 1;
        }
        let (
            listener_ready,
            desktop_events,
            app_events,
            input_ready,
            flip_ready,
            hotplug_ready,
            agent_ready,
        ) = {
            const MAX_POLL_FDS: usize = 5 + MAX_APP_SURFACES + 2;
            let mut descriptors: [PollFd; MAX_POLL_FDS] =
                std::array::from_fn(|_| PollFd::new(self.listener.as_fd(), PollEvents::READ));
            let mut descriptor_count = 0;
            descriptors[descriptor_count] = PollFd::new(self.listener.as_fd(), PollEvents::READ);
            descriptor_count += 1;
            let desktop_polled = self.desktop.is_some();
            if desktop_polled {
                let desktop = self.desktop.as_ref().expect("desktop disappeared");
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
            let flip_offset = descriptor_count;
            descriptors[descriptor_count] = PollFd::new(self.device.as_fd(), PollEvents::READ);
            descriptor_count += 1;
            let hotplug_offset = descriptor_count;
            descriptors[descriptor_count] = PollFd::new(self.hotplug.as_fd(), PollEvents::READ);
            descriptor_count += 1;
            let agent_offset =
                self.append_spice_agent_poll(&mut descriptors, &mut descriptor_count);
            unix::poll(&mut descriptors[..descriptor_count], None)?;
            let listener_ready = descriptors[0].returned().contains(PollEvents::READ);
            let desktop_offset = usize::from(desktop_polled);
            let desktop_events = if desktop_polled {
                descriptors[1].returned()
            } else {
                PollEvents::EMPTY
            };
            let mut app_events = [PollEvents::EMPTY; MAX_APP_SURFACES];
            for (events, descriptor) in app_events[..app_count]
                .iter_mut()
                .zip(&descriptors[1 + desktop_offset..wake_offset])
            {
                *events = descriptor.returned();
            }
            let input_ready = descriptors[wake_offset..flip_offset]
                .iter()
                .any(|descriptor| descriptor.returned() != PollEvents::EMPTY);
            let flip_ready = descriptors[flip_offset].returned() != PollEvents::EMPTY;
            let hotplug_ready = descriptors[hotplug_offset].returned() != PollEvents::EMPTY;
            let agent_ready = descriptors[agent_offset].returned() != PollEvents::EMPTY;
            (
                listener_ready,
                desktop_events,
                app_events,
                input_ready,
                flip_ready,
                hotplug_ready,
                agent_ready,
            )
        };
        let agent_output = if agent_ready {
            self.pump_spice_agent()?
        } else {
            None
        };
        let hotplug_output = if hotplug_ready && self.hotplug.drain_drm_hotplug()? {
            Some(topology_size(self.device.query_topology()?))
        } else {
            None
        };
        let output = agent_output.or(hotplug_output);
        if let Some(size) = output {
            self.configure_output(size)?;
        }
        if listener_ready && let Err(error) = self.accept() {
            eprintln!("compositor: rejected connection: {error}");
        }
        if connection_closed(desktop_events) {
            eprintln!("compositor: desktop disconnected: poll hangup");
            self.reset_epoch();
            self.pending_cursor_shape = None;
            self.pointer_surface = None;
            return Ok(Activity {
                scene: None,
                paint: self.pending_paint.drain(..).collect(),
                move_begin: None,
                input: input_ready,
                flip: flip_ready,
                epoch_reset: true,
                output,
            });
        }
        // 1. HANGUP is a terminal connection transition even when unread frames
        // remain in the socket. Reading one buffered app commit first would
        // publish paint for an owner that can no longer receive Accepted, and
        // that BrokenPipe used to unwind the compositor's DRM owner.
        for (surface_id, events) in app_ids[..app_count]
            .iter()
            .copied()
            .zip(app_events[..app_count].iter().copied())
        {
            if connection_closed(events) {
                eprintln!("compositor: app {surface_id} disconnected: poll hangup");
                self.remove_app(surface_id);
            }
        }
        let mut scene = None;
        let mut move_begin = None;
        if desktop_events.contains(PollEvents::READ) {
            match self.receive_desktop() {
                Ok(messages::DesktopMessage::Scene(accepted)) => scene = Some(accepted),
                Ok(messages::DesktopMessage::Move(request)) => move_begin = Some(request),
                Ok(messages::DesktopMessage::Idle) => {}
                Err(error) => {
                    eprintln!("compositor: desktop disconnected: {error}");
                    self.reset_epoch();
                    self.pending_cursor_shape = None;
                    self.pointer_surface = None;
                    return Ok(Activity {
                        scene: None,
                        paint: self.pending_paint.drain(..).collect(),
                        move_begin: None,
                        input: input_ready,
                        flip: flip_ready,
                        epoch_reset: true,
                        output,
                    });
                }
            }
        }
        for (surface_id, events) in app_ids[..app_count]
            .iter()
            .copied()
            .zip(app_events[..app_count].iter().copied())
        {
            if !connection_closed(events)
                && events.contains(PollEvents::READ)
                && let Err(error) = self.receive_app(surface_id)
            {
                eprintln!("compositor: app {surface_id} disconnected: {error}");
                self.remove_app(surface_id);
                if let Some(accepted) = scene.as_mut() {
                    accepted.revoke_surface(surface_id);
                }
                if move_begin.is_some_and(|request| request.surface_id == surface_id) {
                    move_begin = None;
                }
            }
        }
        Ok(Activity {
            scene,
            paint: self.pending_paint.drain(..).collect(),
            move_begin,
            input: input_ready,
            flip: flip_ready,
            epoch_reset: false,
            output,
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
        let owner = Owner::App(surface_id);
        self.spice_agent.remove_surface(surface_id);
        // 1. Revoke every compositor reference before releasing the app's GPU
        // buffers. Otherwise close can leave input or move composition pointing
        // at an owner that no longer has a stream or pixels.
        revoke_surface_paint(&mut self.pending_paint, surface_id);
        revoke_surface_routing(&mut self.routing, surface_id);
        self.presented_nodes
            .retain(|node| node.window_group != surface_id);
        if self.focused_surface == surface_id {
            self.focused_surface = 0;
        }
        if self.pointer_surface == Some(surface_id) {
            self.set_pointer_surface(None);
        }
        self.clear_pointer_capture(Some(surface_id));
        // 2. No scene, paint, routing, focus or grab owner can now name these
        // resources, so teardown may release them atomically.
        self.buffers
            .values
            .retain(|_, buffer| buffer.owner != owner);
        self.paint.remove_owner(owner);
        self.idle_targets.remove(&owner);
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
        self.paint = PaintStore::new();
        self.pending_paint.clear();
        self.desktop_render_id = None;
        self.idle_targets.clear();
        self.first_scene_presented = false;
        self.routing.clear();
        self.focused_surface = 0;
        self.clear_pointer_capture(None);
        self.move_grab = None;
        self.move_changed = false;
        self.presented_nodes.clear();
        self.desktop_current_buffers.clear();
        self.accelerators.clear();
        self.spice_agent.reset_session();
        self.epoch = self.epoch.wrapping_add(1);
    }
}

fn topology_size(topology: linux_uapi::drm::Topology) -> Size {
    Size {
        width: u32::from(topology.mode.width()),
        height: u32::from(topology.mode.height()),
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

fn connection_closed(events: PollEvents) -> bool {
    events.contains(PollEvents::HANGUP) || events.contains(PollEvents::ERROR)
}

fn revoke_surface_paint(pending: &mut VecDeque<Owner>, surface_id: u32) {
    pending.retain(|owner| *owner != Owner::App(surface_id));
}

fn revoke_surface_routing(routing: &mut Vec<RoutingNode>, surface_id: u32) {
    routing.retain(|node| node.surface_id != surface_id && node.window_group != surface_id);
}

#[cfg(test)]
mod tests;
