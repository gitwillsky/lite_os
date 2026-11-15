//! External XRGB video buffers use standard wl_shm on an independently committed child.

use super::*;
use canvas::Rect;
use std::{fs::File, os::fd::BorrowedFd};
use wayland_wire::protocol::{wl_buffer, wl_shm, wl_shm_pool, wl_subsurface};

/// Producer slots are bounded even when the compositor defers release.
pub const STREAM_SLOTS: usize = 3;

pub(super) struct Stream {
    parent: WindowId,
    bounds: Rect,
    synchronized: bool,
    slots: [Option<Buffer>; STREAM_SLOTS],
}

struct Buffer {
    object: ObjectId,
    generation: u64,
    busy: bool,
}

impl Display {
    /// Creates an input-transparent video child, stacked below its parent's controls.
    /// The parent must clear the video rectangle to transparent when painting.
    /// Returns a protocol error if the server does not offer the required globals.
    pub fn create_stream(&mut self, parent: WindowId, bounds: Rect) -> io::Result<WindowId> {
        if self.viewporter.is_none() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let window = self.create_subsurface(parent, bounds)?;
        let Role::Subsurface { object, .. } = self.windows[&window].role else {
            unreachable!("new stream is a subsurface")
        };
        wl_subsurface::Request::PlaceBelow { sibling: parent.0 }
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)?;
        self.set_input_region(window, Some(Rect::new(0, 0, 0, 0)))?;
        self.set_viewport_destination(window, Some((bounds.width, bounds.height)))?;
        self.streams.insert(
            window,
            Stream {
                parent,
                bounds,
                synchronized: true,
                slots: [const { None }; STREAM_SLOTS],
            },
        );
        Ok(window)
    }

    /// Moves and scales an existing video child. Position takes effect with the parent commit.
    /// Returns whether the geometry changed, or an error for an invalid child or bounds.
    pub fn set_stream_bounds(&mut self, window: WindowId, bounds: Rect) -> io::Result<bool> {
        let stream = self.streams.get(&window).ok_or(io::ErrorKind::NotFound)?;
        if stream.bounds == bounds {
            return Ok(false);
        }
        if !stream.synchronized {
            self.stream_mode(window, wl_subsurface::Request::SetSync)?;
        }
        self.set_subsurface_bounds(window, bounds)?;
        self.set_viewport_destination(window, Some((bounds.width, bounds.height)))?;
        if self.windows[&window].mapped {
            wl_surface::Request::Commit
                .encode(window.0, &mut self.transport.writer)
                .map_err(io::Error::other)?;
        }
        let stream = self.streams.get_mut(&window).expect("validated stream");
        stream.bounds = bounds;
        stream.synchronized = true;
        Ok(true)
    }

    /// Resumes independent video commits after the parent published child geometry.
    /// Encoding or flush failures are returned to the parent owner.
    pub fn stream_parent_committed(&mut self, window: WindowId) -> io::Result<()> {
        let stream = self.streams.get(&window).ok_or(io::ErrorKind::NotFound)?;
        if stream.synchronized {
            self.stream_mode(window, wl_subsurface::Request::SetDesync)?;
            self.transport.flush()?;
            self.streams
                .get_mut(&window)
                .expect("validated stream")
                .synchronized = false;
        }
        Ok(())
    }

    fn stream_mode(&mut self, window: WindowId, request: wl_subsurface::Request) -> io::Result<()> {
        let Role::Subsurface { object, .. } = self.windows[&window].role else {
            unreachable!("stream is a subsurface")
        };
        request
            .encode(object, &mut self.transport.writer)
            .map_err(io::Error::other)
    }

    /// Hides video while preserving the reusable pool slots and pending releases.
    /// Returns standard surface I/O errors.
    pub fn hide_stream(&mut self, window: WindowId) -> io::Result<()> {
        self.unmap_surface(window)?;
        // A compositor may defer the old frame callback while the child is unmapped.
        self.windows
            .get_mut(&window)
            .expect("validated stream")
            .frame = None;
        Ok(())
    }

    /// Attaches a sealed SHM producer slot as XRGB8888, without reading or copying it.
    /// `geometry` is width/height in pixels and stride in bytes. `generation` changes when
    /// the producer replaces the file behind a slot. The fd is borrowed for registration.
    /// Returns false while frame pacing, suspension or an outstanding buffer prevents submit.
    /// A successful submission freezes the producer slot until `StreamBufferReleased`.
    pub fn submit_stream(
        &mut self,
        window: WindowId,
        slot: u32,
        generation: u64,
        fd: BorrowedFd<'_>,
        [width, height, stride]: [i32; 3],
    ) -> io::Result<bool> {
        let index = slot as usize;
        let stream = self.streams.get(&window).ok_or(io::ErrorKind::NotFound)?;
        let previous = stream.slots.get(index).ok_or(io::ErrorKind::InvalidInput)?;
        if previous.as_ref().is_some_and(|buffer| buffer.busy)
            || self.windows[&window].frame.is_some()
            || self.suspended(window)
        {
            return Ok(false);
        }
        let object = match previous {
            Some(buffer) if buffer.generation == generation => buffer.object,
            _ => {
                let extent = stride
                    .checked_mul(height)
                    .filter(|&n| n > 0)
                    .ok_or(io::ErrorKind::InvalidInput)?;
                if width <= 0
                    || height <= 0
                    || width.checked_mul(4).is_none_or(|bytes| bytes > stride)
                    || !linux_uapi::shared_memory::is_shrink_sealed(fd)
                    || File::from(fd.try_clone_to_owned()?).metadata()?.len() < extent as u64
                {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                let pool = self.create_shm_pool(fd, extent as usize)?;
                let object = self.transport.allocate(Interface::WlBuffer)?;
                let writer = &mut self.transport.writer;
                wl_shm_pool::Request::CreateBuffer {
                    id: object,
                    offset: 0,
                    width,
                    height,
                    stride,
                    format: wl_shm::Format::Xrgb8888,
                }
                .encode(pool, writer)
                .map_err(io::Error::other)?;
                // The buffer holds the server-side pool reference after destruction.
                wl_shm_pool::Request::Destroy
                    .encode(pool, writer)
                    .map_err(io::Error::other)?;
                let slot = &mut self
                    .streams
                    .get_mut(&window)
                    .expect("validated stream")
                    .slots[index];
                if let Some(old) = slot.replace(Buffer {
                    object,
                    generation,
                    busy: false,
                }) {
                    wl_buffer::Request::Destroy
                        .encode(old.object, writer)
                        .map_err(io::Error::other)?;
                }
                object
            }
        };
        let callback = self.transport.allocate(Interface::WlCallback)?;
        let writer = &mut self.transport.writer;
        wl_surface::Request::Attach {
            buffer: Some(object),
            x: 0,
            y: 0,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        wl_surface::Request::DamageBuffer {
            x: 0,
            y: 0,
            width,
            height,
        }
        .encode(window.0, writer)
        .map_err(io::Error::other)?;
        let state = self
            .windows
            .get_mut(&window)
            .expect("validated stream window");
        state.mapped = true;
        self.streams
            .get_mut(&window)
            .expect("validated stream")
            .slots[index]
            .as_mut()
            .expect("registered stream buffer")
            .busy = true;
        self.commit_callback(window, callback)?;
        Ok(true)
    }

    pub(super) fn release_stream_buffer(&mut self, object: ObjectId) {
        for stream in self.streams.values_mut() {
            for (slot, entry) in stream.slots.iter_mut().enumerate() {
                if let Some(buffer) = entry
                    .as_mut()
                    .filter(|buffer| buffer.object == object && buffer.busy)
                {
                    buffer.busy = false;
                    self.events.push_back(Event {
                        window: stream.parent,
                        kind: EventKind::StreamBufferReleased(slot as u32),
                    });
                    return;
                }
            }
        }
    }

    pub(super) fn stream_parent(&self, window: WindowId) -> Option<WindowId> {
        self.streams.get(&window).map(|stream| stream.parent)
    }

    pub(super) fn retire_stream(&mut self, window: WindowId) -> io::Result<()> {
        if let Some(stream) = self.streams.remove(&window) {
            for buffer in stream.slots.into_iter().flatten() {
                // No recycled slot is handed to the producer on teardown. Server references
                // live through pending commits, while the producer can drop its own mappings.
                wl_buffer::Request::Destroy
                    .encode(buffer.object, &mut self.transport.writer)
                    .map_err(io::Error::other)?;
            }
        }
        Ok(())
    }
}
