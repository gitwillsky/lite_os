//! Exact display-protocol client for desktop and ordinary app roles.

mod allocation;
mod buffer;
mod clipboard;
mod event;
mod scene;
mod wire;

use std::{
    collections::{HashSet, VecDeque},
    io,
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::UnixStream,
    time::Duration,
};

use display_proto::{
    AcceleratorChord, AcceleratorSet, CloseRequest, Configure, HelloApp, HelloDesktop, MAX_MESSAGE,
    MessageKind, MoveBegin, PROTOCOL_VERSION, PointerPhase, Rect, SetCursorShape, Size,
    SurfaceCommit, Welcome, parse_frame, recv_frame_blocking, send_message,
};
use linux_uapi::drm::{DrmDevice, SharedDumbBuffer};
use linux_uapi::unix::{self, PollEvents, PollFd};

use crate::Mode;
use buffer::Buffer;
pub use event::Event;
use wire::{WireEvent, parse_event, receive_configure};

/// Writable compositor-issued frame.
pub struct Frame<'a> {
    /// Protocol buffer identity used by the next commit.
    pub id: u32,
    /// Mutable premultiplied ARGB8888 mapping.
    pub pixels: &'a mut SharedDumbBuffer,
}

/// One compositor-ready foreign surface emitted by desktop layout. The window
/// frame clip and corner radius live on [`WindowFrame`] (emitted per window),
/// so this carries only the client-area surface geometry.
#[derive(Clone, Debug)]
pub struct ForeignLayer {
    /// App surface identity.
    pub surface_id: u32,
    /// Desktop configure serial represented by these bounds.
    pub configure_serial: u64,
    /// Physical client-area bounds.
    pub bounds: Rect,
    /// Desktop-owned interactive boxes painted after this embedded surface.
    ///
    /// Scene input is independent from pixels: these rectangles restore DOM
    /// stacking for transparent chrome without covering client pixels.
    pub desktop_input: Vec<Rect>,
    /// First desktop hit emitted after the foreign element itself. The renderer
    /// resolves this paint-order boundary before the scene is committed.
    pub(crate) desktop_hit_start: usize,
}

/// One window's frame region, emitted for EVERY `data-lite-window` — including
/// pure-DOM windows (Music Player) with no foreign client surface. It becomes a
/// per-window group `Pixels` scene node so the compositor's move/damage/finish
/// paths, which key on `window_group`, treat every window uniformly. Without it
/// a pure-DOM window has no group node and cannot be moved or erased, leaving a
/// drag ghost.
#[derive(Clone, Copy, Debug)]
pub struct WindowFrame {
    /// Window surface identity (`data-lite-window` value).
    pub surface_id: u32,
    /// Physical outer window rectangle used as the group node's clip.
    pub frame: Rect,
    /// Rounded top-corner radius in physical pixels for the frame clip.
    pub corner_radius: u32,
}

/// One desktop-local global-chrome clip re-painted above every foreign surface
/// so the top bar, dock and open panels stay above window content.
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

/// One exact-version display connection and its compositor-owned buffers.
pub struct Display {
    stream: UnixStream,
    device: DrmDevice,
    physical: Size,
    surface_id: u32,
    configure_serial: u64,
    output_serial: u64,
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
            output_serial: welcome.output_serial,
            buffers: Vec::new(),
            revision: 0,
            ready: HashSet::new(),
            pending: VecDeque::new(),
            submitted: VecDeque::new(),
            accepted: HashSet::new(),
        };
        loop {
            match display.allocate(2, display.physical) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::OutOfMemory
                        && display.adopt_initial_superseding_configure(mode)? => {}
                Err(error) => return Err(error),
            }
        }
        if matches!(mode, Mode::Desktop) {
            // The third desktop buffer is a transient move underlay. Without
            // it the full-screen desktop raster would repaint the moving
            // window at its canonical origin on every damage restoration.
            loop {
                match display.allocate(1, display.physical) {
                    Ok(()) => break,
                    Err(error)
                        if error.kind() == io::ErrorKind::OutOfMemory
                            && display.adopt_initial_superseding_configure(mode)? => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(display)
    }

    fn adopt_initial_superseding_configure(&mut self, mode: &Mode) -> io::Result<bool> {
        let index = match mode {
            Mode::Desktop => self
                .pending
                .iter()
                .rposition(|event| matches!(event, Event::OutputConfigure(_))),
            Mode::App(_) => self
                .pending
                .iter()
                .rposition(|event| matches!(event, Event::Configure(_))),
        };
        let Some(index) = index else {
            return Ok(false);
        };
        let event = self.pending.remove(index).expect("validated pending event");
        match event {
            Event::OutputConfigure(configure) => {
                self.output_serial = configure.serial;
                self.physical = configure.size;
            }
            Event::Configure(configure) => {
                self.configure_serial = configure.serial;
                self.physical = Size {
                    width: configure.width * display_proto::DEVICE_SCALE_FACTOR,
                    height: configure.height * display_proto::DEVICE_SCALE_FACTOR,
                };
            }
            _ => unreachable!("selected configure event"),
        }
        self.pending
            .retain(|event| !matches!(event, Event::OutputConfigure(_) | Event::Configure(_)));
        Ok(true)
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

    /// Adopts one compositor-owned desktop output generation and acquires its
    /// complete presentation triple.
    pub fn reconfigure_output(
        &mut self,
        configure: display_proto::OutputConfigure,
    ) -> io::Result<()> {
        if self.surface_id != 0 || configure.serial <= self.output_serial {
            return Err(invalid("invalid output configure serial"));
        }
        self.output_serial = configure.serial;
        self.physical = configure.size;
        let matching = self
            .buffers
            .iter()
            .filter(|buffer| buffer.matches(configure.size))
            .count();
        let missing = 3usize.saturating_sub(matching);
        if missing > 0 {
            let pair = missing.min(2);
            if let Err(error) = self.allocate(pair as u32, configure.size) {
                if self.output_allocation_was_superseded(&error, configure.serial) {
                    return Ok(());
                }
                return Err(error);
            }
            if missing > pair {
                if let Err(error) = self.allocate((missing - pair) as u32, configure.size) {
                    if self.output_allocation_was_superseded(&error, configure.serial) {
                        return Ok(());
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn output_allocation_was_superseded(&self, error: &io::Error, serial: u64) -> bool {
        error.kind() == io::ErrorKind::OutOfMemory
            && self.pending.iter().any(|event| {
                matches!(event, Event::OutputConfigure(configure) if configure.serial > serial)
            })
    }

    /// Returns the fixed logical CSS viewport.
    pub fn logical_size(&self) -> Size {
        let scale = display_proto::DEVICE_SCALE_FACTOR;
        Size {
            width: self.physical.width.div_ceil(scale),
            height: self.physical.height.div_ceil(scale),
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
        if self.surface_id == 0
            && self
                .pending
                .iter()
                .any(|event| matches!(event, Event::OutputConfigure(_)))
        {
            return Ok(None);
        }
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

    /// Commits one app pixel revision for the active configure.
    ///
    /// # Parameters
    ///
    /// - `buffer_id`: Writable client buffer containing the complete retained
    ///   surface after this revision's raster.
    /// - `damage`: Exact physical surface rectangles changed from the preceding
    ///   revision; an empty list means the pixels are unchanged.
    ///
    /// # Returns
    ///
    /// Returns after the revision has been sent asynchronously.
    ///
    /// # Errors
    ///
    /// Returns an error when revision allocation, encoding or socket delivery
    /// fails.
    pub fn commit_app(&mut self, buffer_id: u32, damage: &[display_proto::Rect]) -> io::Result<()> {
        let revision = self.next_revision()?;
        let mut output = [0u8; MAX_MESSAGE];
        let message = SurfaceCommit::encode(
            &mut output,
            revision,
            self.configure_serial,
            buffer_id,
            damage,
        )
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

    /// Atomically replaces the compositor's global accelerator table.
    ///
    /// Desktop-only: the compositor rejects the message from an app session.
    pub fn set_accelerators(&self, chords: &[AcceleratorChord]) -> io::Result<()> {
        // Header (8) + count (4) + MAX_ACCELERATORS chords of 8 bytes.
        let mut bytes = [0u8; 8 + 4 + 16 * 8];
        let message = AcceleratorSet { chords }
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("accelerator-set encoding failed"))?;
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

    /// Requests the compositor draw one fixed standard cursor shape for this
    /// session's surface.
    pub fn set_cursor_shape(&self, shape: u32) -> io::Result<()> {
        let mut bytes = [0u8; 24];
        let message = SetCursorShape {
            surface_id: self.surface_id,
            shape,
        }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("set-cursor-shape encoding failed"))?;
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
            WireEvent::Retired(id) => {
                self.retire(id)?;
                self.pending.push_back(Event::FrameDone);
                Ok(None)
            }
            event @ (WireEvent::Accepted(_)
            | WireEvent::Discarded(_)
            | WireEvent::Presented { .. }) => {
                let event = self.handle_progress(event)?;
                self.pending.push_back(event);
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
            WireEvent::Retired(id) => {
                self.retire(id)?;
                Ok(Event::FrameDone)
            }
            event @ (WireEvent::Accepted(_)
            | WireEvent::Discarded(_)
            | WireEvent::Presented { .. }) => self.handle_progress(event),
        }
    }

    fn handle_progress(&mut self, event: WireEvent) -> io::Result<Event> {
        match event {
            WireEvent::Accepted(revision) if self.submitted.front().copied() == Some(revision) => {
                self.submitted.pop_front();
                self.accepted.insert(revision);
                Ok(Event::FrameDone)
            }
            WireEvent::Discarded(revision) if self.submitted.front().copied() == Some(revision) => {
                self.submitted.pop_front();
                self.accepted.remove(&revision);
                Ok(Event::FrameDone)
            }
            WireEvent::Discarded(revision) if self.accepted.remove(&revision) => {
                Ok(Event::FrameDone)
            }
            WireEvent::Presented {
                revision,
                monotonic_ns,
            } if self.accepted.remove(&revision) => Ok(Event::Presented { monotonic_ns }),
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

    fn next_revision(&mut self) -> io::Result<u64> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| io::Error::other("visual revision exhausted"))?;
        Ok(self.revision)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
