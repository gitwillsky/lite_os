//! Strict multi-process display session and compositor-owned client buffers.

mod buffers;
mod routing;
mod scene;
mod wire;

pub use buffers::Buffers;
pub use scene::Scene;
use buffers::Owner;
use wire::{new_epoch, receive, send_accepted, valid_app_id};

use std::{
    collections::HashMap,
    fs, io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::{UnixListener, UnixStream},
    time::Duration,
};

use display_proto::{
    AppClosed, AppOpened, BufferAlloc, CloseRequest, Configure, ConfigureReady, HelloApp,
    HelloDesktop, MAX_APP_SURFACES, MAX_MESSAGE, MessageKind, PROTOCOL_VERSION, Rect, Size,
    SurfaceCommit, Welcome, parse_frame, recv_frame_blocking, send_message, send_message_with_fd,
};
use linux_uapi::{
    drm::DrmDevice,
    unix::{self, PollEvents, PollFd},
};

/// One presented surface's hit-test region, retained for input routing.
#[derive(Clone)]
struct RoutingNode {
    surface_id: u32,
    bounds: Rect,
    input: Vec<Rect>,
}

struct Desktop {
    stream: UnixStream,
    last_revision: u64,
}

#[derive(Clone, Copy)]
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
    pointer_capture: Option<(u32, Rect)>,
}

/// Outcome of one [`Session::poll`] wait.
pub struct Activity {
    /// A newly accepted desktop scene ready to compose, if any.
    pub scene: Option<Scene>,
    /// Whether a caller-supplied wake descriptor (evdev) became readable.
    pub input: bool,
}

impl Session {
    /// Creates the only display socket and starts an empty epoch.
    pub fn open(device: &DrmDevice, display: Size) -> io::Result<Self> {
        let _ = fs::remove_file(display_proto::SOCKET_PATH);
        let listener = UnixListener::bind(display_proto::SOCKET_PATH)?;
        listener.set_nonblocking(true)?;
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
        wake: &[BorrowedFd<'_>],
        timeout: Duration,
    ) -> io::Result<Activity> {
        let app_ids: Vec<u32> = self.apps.keys().copied().collect();
        let (listener_ready, desktop_ready, app_ready, input_ready) = {
            let mut descriptors = Vec::with_capacity(2 + app_ids.len() + wake.len());
            descriptors.push(PollFd::new(self.listener.as_fd(), PollEvents::READ));
            if let Some(desktop) = &self.desktop {
                descriptors.push(PollFd::new(desktop.stream.as_fd(), PollEvents::READ));
            }
            for id in &app_ids {
                descriptors.push(PollFd::new(self.apps[id].stream.as_fd(), PollEvents::READ));
            }
            let wake_offset = descriptors.len();
            for fd in wake {
                descriptors.push(PollFd::new(*fd, PollEvents::READ));
            }
            unix::poll(&mut descriptors, Some(timeout))?;
            let listener_ready = descriptors[0].returned().contains(PollEvents::READ);
            let desktop_offset = usize::from(self.desktop.is_some());
            let desktop_ready =
                self.desktop.is_some() && descriptors[1].returned() != PollEvents::EMPTY;
            let app_ready = descriptors[1 + desktop_offset..wake_offset]
                .iter()
                .map(|descriptor| descriptor.returned() != PollEvents::EMPTY)
                .collect::<Vec<_>>();
            let input_ready = descriptors[wake_offset..]
                .iter()
                .any(|descriptor| descriptor.returned() != PollEvents::EMPTY);
            (listener_ready, desktop_ready, app_ready, input_ready)
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
                    return Ok(Activity {
                        scene: None,
                        input: input_ready,
                    });
                }
            }
        }
        for (surface_id, ready) in app_ids.into_iter().zip(app_ready) {
            if ready && let Err(error) = self.receive_app(surface_id) {
                eprintln!("compositor: app {surface_id} disconnected: {error}");
                self.remove_app(surface_id);
            }
        }
        Ok(Activity {
            scene,
            input: input_ready,
        })
    }

    fn accept(&mut self) -> io::Result<()> {
        let (stream, _) = match self.listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut bytes = [0u8; MAX_MESSAGE];
        let (length, fd) = recv_frame_blocking(&stream, &mut bytes)?;
        if fd.is_some() || length == 0 {
            return Err(invalid("invalid display handshake"));
        }
        let frame = parse_frame(&bytes[..length]).ok_or_else(|| invalid("invalid handshake"))?;
        match frame.kind() {
            MessageKind::HelloDesktop => {
                HelloDesktop::parse(frame.payload())
                    .ok_or_else(|| invalid("desktop protocol mismatch"))?;
                if self.desktop.is_some() {
                    return Err(invalid("desktop already connected"));
                }
                self.welcome(&stream, 0)?;
                self.desktop = Some(Desktop {
                    stream,
                    last_revision: 0,
                });
                eprintln!("compositor: desktop connected");
            }
            MessageKind::HelloApp => {
                let hello = HelloApp::parse(frame.payload())
                    .ok_or_else(|| invalid("app protocol mismatch"))?;
                let id = std::str::from_utf8(hello.app_id)
                    .ok()
                    .filter(|id| valid_app_id(id))
                    .ok_or_else(|| invalid("invalid app id"))?
                    .to_owned();
                if self.desktop.is_none() || self.apps.len() >= MAX_APP_SURFACES {
                    return Err(invalid("app session unavailable"));
                }
                let surface_id = self.take_surface_id()?;
                self.welcome(&stream, surface_id)?;
                self.apps.insert(
                    surface_id,
                    App {
                        stream,
                        id,
                        configure: None,
                        last_revision: 0,
                        pending: None,
                        current: None,
                    },
                );
                self.notify_opened(surface_id)?;
                eprintln!("compositor: app {surface_id} connected");
            }
            _ => return Err(invalid("handshake role required")),
        }
        Ok(())
    }

    fn welcome(&self, stream: &UnixStream, surface_id: u32) -> io::Result<()> {
        let mut bytes = [0u8; 64];
        let message = Welcome {
            version: PROTOCOL_VERSION,
            display: self.display,
            surface_id,
            session_epoch: self.epoch,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("welcome encoding failed"))?;
        send_message_with_fd(stream, message, self.device.as_fd())
    }

    fn receive_desktop(&mut self) -> io::Result<Option<Scene>> {
        let (kind, payload) = receive(self.desktop_stream()?)?;
        match kind {
            MessageKind::BufferAlloc => {
                self.allocate(
                    Owner::Desktop,
                    BufferAlloc::parse(&payload).ok_or_else(|| invalid("invalid allocation"))?,
                )?;
                Ok(None)
            }
            MessageKind::Configure => {
                let configure =
                    Configure::parse(&payload).ok_or_else(|| invalid("invalid configure"))?;
                self.route_configure(configure)?;
                Ok(None)
            }
            MessageKind::CloseRequest => {
                let request = CloseRequest::parse(&payload)
                    .ok_or_else(|| invalid("invalid close request"))?;
                self.route_close(request.surface_id)?;
                Ok(None)
            }
            MessageKind::SceneCommit => self.accept_scene(&payload).map(Some),
            _ => Err(invalid("message is invalid for desktop role")),
        }
    }

    fn receive_app(&mut self, surface_id: u32) -> io::Result<()> {
        let stream = &self
            .apps
            .get(&surface_id)
            .ok_or_else(|| invalid("unknown app"))?
            .stream;
        let (kind, payload) = receive(stream)?;
        match kind {
            MessageKind::BufferAlloc => self.allocate(
                Owner::App(surface_id),
                BufferAlloc::parse(&payload).ok_or_else(|| invalid("invalid allocation"))?,
            ),
            MessageKind::SurfaceCommit => self.accept_surface(
                surface_id,
                SurfaceCommit::parse(&payload).ok_or_else(|| invalid("invalid surface commit"))?,
            ),
            _ => Err(invalid("message is invalid for app role")),
        }
    }

    fn route_configure(&mut self, configure: Configure) -> io::Result<()> {
        let app = self
            .apps
            .get_mut(&configure.surface_id)
            .ok_or_else(|| invalid("configure targets unknown app"))?;
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
        let app = self
            .apps
            .get(&surface_id)
            .ok_or_else(|| invalid("close targets unknown app"))?;
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
        let configure = app
            .configure
            .filter(|configure| configure.serial == commit.configure_serial)
            .ok_or_else(|| invalid("surface commit configure mismatch"))?;
        let buffer = self
            .buffers
            .values
            .get(&commit.buffer_id)
            .ok_or_else(|| invalid("unknown app buffer"))?;
        if commit.revision <= app.last_revision
            || app.pending.is_some()
            || buffer.owner != Owner::App(surface_id)
            || buffer.busy
            || buffer.size.width != configure.width * display_proto::DEVICE_SCALE_FACTOR
            || buffer.size.height != configure.height * display_proto::DEVICE_SCALE_FACTOR
        {
            return Err(invalid("surface commit state invalid"));
        }
        let content = Content {
            revision: commit.revision,
            configure_serial: commit.configure_serial,
            buffer_id: commit.buffer_id,
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
