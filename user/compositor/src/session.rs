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
pub(crate) use buffers::Owner;
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
    MAX_APP_SURFACES, Rect, SetCursorShape, Size, TextureFormat, send_message,
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
    /// GPU-rendered desktop snapshots with one window group omitted.
    move_underlays: HashMap<u32, u32>,
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
    /// Whether a caller-supplied wake descriptor (evdev) became readable.
    pub input: bool,
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
            move_underlays: HashMap::new(),
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

    pub(crate) fn paint_texture(
        &self,
        owner: Owner,
        texture_id: u32,
    ) -> Option<(&VirglResource, TextureFormat)> {
        self.paint.texture(owner, texture_id)
    }

    pub(crate) fn complete_paint(&mut self, owner: Owner, target: VirglResource) -> io::Result<()> {
        let list = self
            .paint
            .list(owner)
            .ok_or_else(|| invalid("display list disappeared"))?;
        let revision = list.revision;
        let configuration_serial = list.configuration_serial;
        let size = Size {
            width: target.width(),
            height: target.height(),
        };
        let id = self.take_buffer_id()?;
        self.buffers.values.insert(
            id,
            buffers::Buffer {
                pixels: target,
                size,
                owner,
                busy: false,
            },
        );
        match owner {
            Owner::Desktop => {
                if self.move_grab.is_none() {
                    for (_, id) in self.move_underlays.drain() {
                        self.buffers.values.remove(&id);
                    }
                }
                if let Some(previous) = self.desktop_render_id.replace(id)
                    && !self.desktop_current_buffers.contains(&previous)
                {
                    self.buffers.values.remove(&previous);
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
                    self.buffers.values.remove(&previous);
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

    pub(crate) fn has_move_underlay(&self, group: u32) -> bool {
        self.move_underlays.contains_key(&group)
    }

    pub(crate) fn install_move_underlay(
        &mut self,
        group: u32,
        target: VirglResource,
    ) -> io::Result<()> {
        let id = self.take_buffer_id()?;
        let size = Size {
            width: target.width(),
            height: target.height(),
        };
        self.buffers.values.insert(
            id,
            buffers::Buffer {
                pixels: target,
                size,
                owner: Owner::Desktop,
                busy: false,
            },
        );
        if let Some(previous) = self.move_underlays.insert(group, id) {
            self.buffers.values.remove(&previous);
        }
        Ok(())
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
        let (listener_ready, desktop_ready, app_ready, input_ready, hotplug_ready, agent_ready) = {
            const MAX_POLL_FDS: usize = 4 + MAX_APP_SURFACES + 2;
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
            let hotplug_offset = descriptor_count;
            descriptors[descriptor_count] = PollFd::new(self.hotplug.as_fd(), PollEvents::READ);
            descriptor_count += 1;
            let agent_offset =
                self.append_spice_agent_poll(&mut descriptors, &mut descriptor_count);
            unix::poll(&mut descriptors[..descriptor_count], None)?;
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
            let input_ready = descriptors[wake_offset..hotplug_offset]
                .iter()
                .any(|descriptor| descriptor.returned() != PollEvents::EMPTY);
            let hotplug_ready = descriptors[hotplug_offset].returned() != PollEvents::EMPTY;
            let agent_ready = descriptors[agent_offset].returned() != PollEvents::EMPTY;
            (
                listener_ready,
                desktop_ready,
                app_ready,
                input_ready,
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
                        paint: self.pending_paint.drain(..).collect(),
                        input: input_ready,
                        epoch_reset: true,
                        output,
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
        Ok(Activity {
            scene,
            paint: self.pending_paint.drain(..).collect(),
            input: input_ready,
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
        self.spice_agent.remove_surface(surface_id);
        self.buffers
            .values
            .retain(|_, buffer| buffer.owner != Owner::App(surface_id));
        self.paint.remove_owner(Owner::App(surface_id));
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
        self.paint = PaintStore::new();
        self.pending_paint.clear();
        self.desktop_render_id = None;
        self.first_scene_presented = false;
        self.routing.clear();
        self.focused_surface = 0;
        self.clear_pointer_capture(None);
        self.move_grab = None;
        self.move_damage = None;
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

#[cfg(test)]
mod tests;
