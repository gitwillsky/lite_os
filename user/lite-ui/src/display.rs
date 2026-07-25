//! Exact display-protocol client for desktop and ordinary app roles.

mod wire;

use std::{
    collections::{HashSet, VecDeque},
    io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::UnixStream,
    time::Duration,
};

use display_proto::{
    BufferAlloc, BufferAllocated, BufferRelease, CloseRequest, Configure, HelloApp, HelloDesktop,
    InputKey, InputPointer, MAX_MESSAGE, MessageKind, MoveBegin, PROTOCOL_VERSION, PointerPhase,
    Rect, Rectangles, SceneCommit, SceneNode, SceneNodeKind, Size, SurfaceCommit, Welcome,
    parse_frame, recv_frame_blocking, send_message,
};
use linux_uapi::drm::{DrmDevice, SharedDumbBuffer};
use linux_uapi::unix::{self, PollEvents, PollFd};

use crate::Mode;
use wire::{WireEvent, parse_event, receive_configure};

struct Buffer {
    id: u32,
    pixels: SharedDumbBuffer,
    free: bool,
}

impl Buffer {
    /// Buffers sized for a superseded configure can never be presented again:
    /// they retire instead of recycling back into the free pool.
    fn matches(&self, physical: Size) -> bool {
        self.pixels.width() == physical.width as usize
            && self.pixels.height() == physical.height as usize
    }
}

/// Writable compositor-issued frame.
pub struct Frame<'a> {
    /// Protocol buffer identity used by the next commit.
    pub id: u32,
    /// Mutable premultiplied ARGB8888 mapping.
    pub pixels: &'a mut SharedDumbBuffer,
}

/// One compositor-ready foreign surface emitted by desktop layout.
#[derive(Clone, Copy, Debug)]
pub struct ForeignLayer {
    /// App surface identity.
    pub surface_id: u32,
    /// Desktop configure serial represented by these bounds.
    pub configure_serial: u64,
    /// Physical client-area bounds.
    pub bounds: Rect,
    /// Physical window frame clip re-painted above lower foreign content, so
    /// each window's chrome and content stack as one atomic layer.
    pub frame: Rect,
    /// Rounded-corner radius in physical pixels for the frame clip; the compositor
    /// skips corner pixels so lower content shows through the Luna-style rounded top corners instead of stale chrome pixels.
    pub corner_radius: u32,
}

/// One desktop-local chrome clip (taskbar, Start menu) re-painted above every
/// foreign surface so it stays on top of window content.
#[derive(Clone, Copy, Debug)]
pub struct Overlay {
    /// Physical clip rectangle re-copied from the desktop buffer.
    pub rect: Rect,
    /// Rounded top-corner radius in physical pixels; the compositor skips the
    /// corner cutout so lower window content shows through instead of a square
    /// wallpaper corner.
    pub corner_radius: u32,
    /// CSS `z-index` of the fixed element; overlays are stable-sorted ascending
    /// so higher-`z-index` chrome re-blits last (on top).
    pub z_index: i32,
}

/// One validated asynchronous display event.
#[derive(Clone, Debug)]
pub enum Event {
    /// Ordinary app published a top-level surface.
    AppOpened { surface_id: u32, app_id: String },
    /// Ordinary app removed its top-level surface.
    AppClosed { surface_id: u32 },
    /// A pointer-down hit a foreign surface; the desktop should raise it.
    SurfaceActivated { surface_id: u32 },
    /// A compositor-side move ended at one canonical logical position.
    MoveComplete { surface_id: u32, x: i32, y: i32 },
    /// App pixels for one desktop configure are ready.
    ConfigureReady { surface_id: u32, serial: u64 },
    /// Desktop selected a new app client size.
    Configure(Configure),
    /// Desktop requested app termination.
    Close,
    /// Pointer input routed against the presented scene.
    Pointer(InputPointer),
    /// Keyboard input routed to the presented focused surface.
    Key(InputKey),
    /// An asynchronous submit/release/presentation transition freed pipeline progress.
    FrameDone,
}

/// One exact-version display connection and its compositor-owned buffers.
pub struct Display {
    stream: UnixStream,
    device: DrmDevice,
    physical: Size,
    surface_id: u32,
    configure_serial: u64,
    buffers: Vec<Buffer>,
    revision: u64,
    ready: HashSet<(u32, u64)>,
    pending: VecDeque<Event>,
    submitted: VecDeque<u64>,
    accepted: HashSet<u64>,
}

impl Display {
    /// Connects, fixes the role and acquires the presentation pair plus the
    /// desktop-only move-underlay scratch.
    pub fn open(mode: &Mode) -> io::Result<Self> {
        let stream = UnixStream::connect(display_proto::SOCKET_PATH)?;
        let mut bytes = [0u8; 128];
        let hello = match mode {
            Mode::Desktop => HelloDesktop {
                version: PROTOCOL_VERSION,
            }
            .encode(&mut bytes),
            Mode::App(id) => HelloApp {
                version: PROTOCOL_VERSION,
                app_id: id.as_bytes(),
            }
            .encode(&mut bytes),
        }
        .ok_or_else(|| io::Error::other("display handshake encoding failed"))?;
        send_message(&stream, hello)?;
        let mut input = [0u8; MAX_MESSAGE];
        let (length, fd) = recv_frame_blocking(&stream, &mut input)?;
        let frame = parse_frame(&input[..length])
            .filter(|frame| frame.kind() == MessageKind::Welcome)
            .ok_or_else(|| invalid("display welcome missing"))?;
        let welcome = Welcome::parse(frame.payload()).ok_or_else(|| invalid("invalid welcome"))?;
        let device = DrmDevice::from_owned_fd(fd.ok_or_else(|| invalid("DRM descriptor missing"))?);
        let (physical, configure_serial) = match mode {
            Mode::Desktop => (welcome.display, 0),
            Mode::App(_) => {
                let configure = receive_configure(&stream, welcome.surface_id)?;
                (
                    Size {
                        width: configure.width * display_proto::DEVICE_SCALE_FACTOR,
                        height: configure.height * display_proto::DEVICE_SCALE_FACTOR,
                    },
                    configure.serial,
                )
            }
        };
        let mut display = Self {
            stream,
            device,
            physical,
            surface_id: welcome.surface_id,
            configure_serial,
            buffers: Vec::new(),
            revision: 0,
            ready: HashSet::new(),
            pending: VecDeque::new(),
            submitted: VecDeque::new(),
            accepted: HashSet::new(),
        };
        display.allocate(2, physical)?;
        if matches!(mode, Mode::Desktop) {
            // The third desktop buffer is a transient move underlay. Without
            // it the full-screen desktop raster would repaint the moving
            // window at its canonical origin on every damage restoration.
            display.allocate(1, physical)?;
        }
        Ok(display)
    }

    /// Adopts one desktop-issued configure and tops the buffer pair back up.
    ///
    /// Buffers already matching the new size survive (repeated toggles reuse
    /// them); mismatched ones retire as their compositor releases arrive.
    pub fn reconfigure(&mut self, configure: Configure) -> io::Result<()> {
        let physical = Size {
            width: configure.width * display_proto::DEVICE_SCALE_FACTOR,
            height: configure.height * display_proto::DEVICE_SCALE_FACTOR,
        };
        self.configure_serial = configure.serial;
        if physical == self.physical {
            return Ok(());
        }
        self.physical = physical;
        let matching = self
            .buffers
            .iter()
            .filter(|buffer| buffer.matches(physical))
            .count();
        let missing = 2usize.saturating_sub(matching);
        if missing > 0 {
            // A rapid resize emits Configure serials faster than a buffer alloc
            // round-trips: by the time this request reaches the compositor, a
            // newer Configure has already superseded the size, so the compositor
            // rejects the allocation (geometry mismatch, error 22 -> OutOfMemory
            // kind here). That is a transient race, not a fatal condition — drop
            // this reconfigure's buffer top-up and keep the current buffers; the
            // next Configure (already in flight) reconciles the size. Only the
            // rejection is swallowed; framing/mapping errors stay fatal.
            if let Err(error) = self.allocate(missing as u32, physical) {
                if error.kind() == io::ErrorKind::OutOfMemory {
                    eprintln!("lite-ui: buffer request rejected (resize race), dropping frame");
                    return Ok(());
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Returns the fixed logical CSS viewport.
    pub fn logical_size(&self) -> Size {
        Size {
            width: self.physical.width / display_proto::DEVICE_SCALE_FACTOR,
            height: self.physical.height / display_proto::DEVICE_SCALE_FACTOR,
        }
    }

    /// Returns the display socket for the owning event loop's readiness poll.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// Returns whether commit acknowledgement handling already queued an event.
    pub fn has_pending_event(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Acquires one released writable buffer for the active configure size.
    pub fn acquire(&mut self) -> io::Result<Option<Frame<'_>>> {
        let physical = self.physical;
        let Some(buffer) = self
            .buffers
            .iter_mut()
            .find(|buffer| buffer.free && buffer.matches(physical))
        else {
            return Ok(None);
        };
        buffer.free = false;
        Ok(Some(Frame {
            id: buffer.id,
            pixels: &mut buffer.pixels,
        }))
    }

    /// Commits desktop pixels interleaved with ready app surface layers.
    ///
    /// Node order is the z-stack: the full desktop buffer, then per window its
    /// frame clip re-painted above lower foreign content followed by its own
    /// surface, and finally overlay chrome clips (taskbar/menus) above all
    /// content. One window's content can never cover another window's chrome.
    pub fn commit_desktop(
        &mut self,
        buffer_id: u32,
        focused_surface: u32,
        foreign: &[ForeignLayer],
        overlays: &[Overlay],
        pixels_changed: bool,
    ) -> io::Result<()> {
        let revision = self.next_revision()?;
        let full = Rect {
            x: 0,
            y: 0,
            width: self.physical.width,
            height: self.physical.height,
        };
        let full_input = [full];
        let full_damage = [full];
        let no_damage = [];
        let pixel_damage = if pixels_changed {
            Rectangles::from_slice(&full_damage)
        } else {
            Rectangles::from_slice(&no_damage)
        };
        let mut nodes = Vec::with_capacity(1 + foreign.len() * 2 + overlays.len());
        nodes.push(SceneNode {
            kind: SceneNodeKind::Pixels,
            window_group: 0,
            source_id: buffer_id,
            corner_radius: 0,
            configure_serial: 0,
            bounds: full,
            clip: full,
            opaque: Some(full),
            input: Rectangles::from_slice(&full_input),
            damage: pixel_damage,
        });
        let foreign_bounds: Vec<[Rect; 1]> = foreign.iter().map(|layer| [layer.bounds]).collect();
        let foreign_frames: Vec<[Rect; 1]> = foreign.iter().map(|layer| [layer.frame]).collect();
        for (layer, (bounds_input, frame_input)) in foreign
            .iter()
            .zip(foreign_bounds.iter().zip(&foreign_frames))
        {
            if !self
                .ready
                .contains(&(layer.surface_id, layer.configure_serial))
            {
                continue;
            }
            nodes.push(SceneNode {
                kind: SceneNodeKind::Pixels,
                window_group: layer.surface_id,
                source_id: buffer_id,
                corner_radius: layer.corner_radius,
                configure_serial: 0,
                bounds: full,
                clip: layer.frame,
                opaque: None,
                input: Rectangles::from_slice(frame_input),
                damage: Rectangles::from_slice(&no_damage),
            });
            nodes.push(SceneNode {
                kind: SceneNodeKind::ForeignSurface,
                window_group: layer.surface_id,
                source_id: layer.surface_id,
                corner_radius: 0,
                configure_serial: layer.configure_serial,
                bounds: layer.bounds,
                clip: full,
                opaque: Some(layer.bounds),
                input: Rectangles::from_slice(bounds_input),
                damage: Rectangles::from_slice(&no_damage),
            });
        }
        let overlay_inputs: Vec<[Rect; 1]> =
            overlays.iter().map(|overlay| [overlay.rect]).collect();
        for (overlay, input) in overlays.iter().zip(&overlay_inputs) {
            nodes.push(SceneNode {
                kind: SceneNodeKind::Pixels,
                window_group: 0,
                source_id: buffer_id,
                corner_radius: overlay.corner_radius,
                configure_serial: 0,
                bounds: full,
                clip: overlay.rect,
                opaque: None,
                input: Rectangles::from_slice(input),
                damage: Rectangles::from_slice(&no_damage),
            });
        }
        let mut output = [0u8; MAX_MESSAGE];
        let message = SceneCommit::encode(&mut output, revision, focused_surface, &nodes)
            .ok_or_else(|| io::Error::other("scene encoding failed"))?;
        send_message(&self.stream, message)?;
        self.submitted.push_back(revision);
        Ok(())
    }

    /// Commits one app pixel revision for the active configure.
    pub fn commit_app(&mut self, buffer_id: u32) -> io::Result<()> {
        let revision = self.next_revision()?;
        let mut output = [0u8; MAX_MESSAGE];
        let message =
            SurfaceCommit::encode(&mut output, revision, self.configure_serial, buffer_id, &[])
                .ok_or_else(|| io::Error::other("surface encoding failed"))?;
        send_message(&self.stream, message)?;
        self.submitted.push_back(revision);
        Ok(())
    }

    /// Sends one desktop-owned configure to its app surface.
    pub fn configure(&self, configure: Configure) -> io::Result<()> {
        let mut bytes = [0u8; 40];
        let message = configure
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("configure encoding failed"))?;
        send_message(&self.stream, message)
    }

    /// Routes an unconditional desktop close request.
    pub fn close(&self, surface_id: u32) -> io::Result<()> {
        let mut bytes = [0u8; 24];
        let message = CloseRequest { surface_id }
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("close encoding failed"))?;
        send_message(&self.stream, message)
    }

    /// Authorizes a compositor-side move using the exact pointer-down serial.
    pub fn begin_move(&self, request: MoveBegin) -> io::Result<()> {
        let mut bytes = [0u8; 48];
        let message = request
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("move-begin encoding failed"))?;
        send_message(&self.stream, message)
    }

    /// Blocks until the next validated asynchronous event.
    ///
    /// Successive pointer motions coalesce into the newest one: a drag
    /// generates motion far faster than one React render plus presented wait
    /// per event can drain, and dispatching every stale position would lag
    /// the window behind the cursor. Collapsing stops at the first non-motion
    /// event so button transitions and lifecycle events keep exact ordering.
    pub fn next_event(&mut self) -> io::Result<Event> {
        let mut event = self.next_wire_event()?;
        while matches!(event, Event::Pointer(pointer) if pointer.phase == PointerPhase::Motion) {
            let Some(newer) = self.take_queued_motion()? else {
                break;
            };
            event = newer;
        }
        Ok(event)
    }

    /// Returns the next motion only when one is already buffered or
    /// immediately readable, never blocking and never consuming a non-motion
    /// event ahead of it.
    fn take_queued_motion(&mut self) -> io::Result<Option<Event>> {
        if let Some(event) = self.pending.front() {
            let motion =
                matches!(event, Event::Pointer(pointer) if pointer.phase == PointerPhase::Motion);
            return if motion {
                Ok(self.pending.pop_front())
            } else {
                Ok(None)
            };
        }
        if !self.socket_readable()? {
            return Ok(None);
        }
        match self.receive()? {
            WireEvent::Public(event @ Event::Pointer(pointer))
                if pointer.phase == PointerPhase::Motion =>
            {
                Ok(Some(event))
            }
            WireEvent::Public(Event::ConfigureReady { surface_id, serial }) => {
                self.ready.insert((surface_id, serial));
                self.pending
                    .push_back(Event::ConfigureReady { surface_id, serial });
                Ok(None)
            }
            WireEvent::Public(event) => {
                self.pending.push_back(event);
                Ok(None)
            }
            WireEvent::Released(id) => {
                self.release(id)?;
                self.pending.push_back(Event::FrameDone);
                Ok(None)
            }
            event @ (WireEvent::Accepted(_) | WireEvent::Presented(_)) => {
                self.handle_progress(event)?;
                self.pending.push_back(Event::FrameDone);
                Ok(None)
            }
        }
    }

    /// Reports whether at least one wire frame is readable without blocking.
    fn socket_readable(&self) -> io::Result<bool> {
        let mut descriptors = [PollFd::new(self.as_fd(), PollEvents::READ)];
        unix::poll(&mut descriptors, Some(Duration::ZERO))?;
        Ok(descriptors[0].returned() != PollEvents::EMPTY)
    }

    fn next_wire_event(&mut self) -> io::Result<Event> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        match self.receive()? {
            WireEvent::Public(Event::ConfigureReady { surface_id, serial }) => {
                self.ready.insert((surface_id, serial));
                Ok(Event::ConfigureReady { surface_id, serial })
            }
            WireEvent::Public(event) => Ok(event),
            WireEvent::Released(id) => {
                self.release(id)?;
                Ok(Event::FrameDone)
            }
            event @ (WireEvent::Accepted(_) | WireEvent::Presented(_)) => {
                self.handle_progress(event)?;
                Ok(Event::FrameDone)
            }
        }
    }

    fn handle_progress(&mut self, event: WireEvent) -> io::Result<()> {
        match event {
            WireEvent::Accepted(revision) if self.submitted.front().copied() == Some(revision) => {
                self.submitted.pop_front();
                self.accepted.insert(revision);
                Ok(())
            }
            WireEvent::Presented(revision) if self.accepted.remove(&revision) => Ok(()),
            _ => Err(invalid("display acknowledgement ordering failed")),
        }
    }

    fn receive(&self) -> io::Result<WireEvent> {
        let mut bytes = [0u8; MAX_MESSAGE];
        let (length, fd) = recv_frame_blocking(&self.stream, &mut bytes)?;
        if length == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "display EOF"));
        }
        if fd.is_some() {
            return Err(invalid("unexpected descriptor"));
        }
        let frame =
            parse_frame(&bytes[..length]).ok_or_else(|| invalid("invalid display event"))?;
        parse_event(frame.kind(), frame.payload(), self.surface_id)
            .ok_or_else(|| invalid("invalid display event role"))
    }

    fn release(&mut self, id: u32) -> io::Result<()> {
        // A release naming a buffer this surface no longer tracks is a
        // disconnect/reconfigure race, not corruption: during a rapid resize the
        // compositor can retire a buffer whose mapping the app already dropped
        // (e.g. after a configure swapped the surface size). Nothing to free —
        // drop the release rather than abort the whole app under panic=abort.
        let Some(index) = self.buffers.iter().position(|buffer| buffer.id == id) else {
            return Ok(());
        };
        if !self.buffers[index].matches(self.physical) {
            // Retired buffer: the compositor destroyed its twin, so the
            // release carries "drop the mapping", not "back to the pool".
            self.buffers.remove(index);
            return Ok(());
        }
        let buffer = &mut self.buffers[index];
        if buffer.free {
            return Err(invalid("buffer released twice"));
        }
        buffer.free = true;
        Ok(())
    }

    fn next_revision(&mut self) -> io::Result<u64> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("visual revision exhausted"))?;
        Ok(self.revision)
    }
}

impl Display {
    /// Allocates `count` fresh compositor buffers for one configure size.
    ///
    /// The wait loop keeps applying retirement releases that overtake the
    /// allocation response: reconfigure-time cleanup on the compositor sends
    /// exactly those before answering, and flip-driven releases may land here
    /// on any later top-up.
    fn allocate(&mut self, count: u32, physical: Size) -> io::Result<()> {
        let mut bytes = [0u8; 128];
        let request = BufferAlloc {
            request_id: 1,
            size: physical,
            count,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("buffer request encoding failed"))?;
        send_message(&self.stream, request)?;
        let mut input = [0u8; MAX_MESSAGE];
        let allocated = loop {
            let (length, fd) = recv_frame_blocking(&self.stream, &mut input)?;
            if fd.is_some() {
                return Err(invalid("buffer response carried a descriptor"));
            }
            let frame =
                parse_frame(&input[..length]).ok_or_else(|| invalid("invalid display event"))?;
            match frame.kind() {
                MessageKind::BufferAllocated => {
                    break BufferAllocated::parse(frame.payload())
                        .filter(|response| {
                            response.request_id == 1
                                && response.error == 0
                                && response.count == count
                        })
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::OutOfMemory, "buffer request rejected")
                        })?;
                }
                MessageKind::BufferRelease => {
                    let id = BufferRelease::parse(frame.payload())
                        .ok_or_else(|| invalid("invalid buffer release"))?
                        .buffer_id;
                    self.release(id)?;
                }
                kind => {
                    // A synchronous alloc round-trip can be interleaved with any
                    // legitimate async event during rapid resize/render churn —
                    // a newer Configure, an Accepted/Presented progress ack, or
                    // input. These are not "missing responses": route them the
                    // same way the main loop would (queue public events, fold
                    // progress acks) and keep waiting for BufferAllocated, rather
                    // than aborting the app under panic=abort. Only a truly
                    // unparseable frame stays fatal.
                    match parse_event(kind, frame.payload(), self.surface_id)
                        .ok_or_else(|| invalid("buffer response missing"))?
                    {
                        WireEvent::Public(event) => self.pending.push_back(event),
                        WireEvent::Released(id) => self.release(id)?,
                        progress @ (WireEvent::Accepted(_) | WireEvent::Presented(_)) => {
                            self.handle_progress(progress)?;
                        }
                    }
                }
            }
        };
        for descriptor in allocated.buffers.iter().take(count as usize) {
            self.buffers.push(Buffer {
                id: descriptor.buffer_id,
                pixels: self.device.map_shared_dumb(
                    descriptor.gem_handle,
                    physical.width as usize,
                    physical.height as usize,
                    descriptor.pitch as usize,
                    descriptor.byte_len as usize,
                )?,
                free: true,
            });
        }
        Ok(())
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
