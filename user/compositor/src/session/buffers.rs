//! Compositor-owned client buffer allocation and quota accounting.

use std::io;

use display_proto::{
    BufferAlloc, BufferAllocated, BufferDescriptor, MAX_CONNECTION_FRAME_EQUIVALENTS,
    MAX_SESSION_FRAME_EQUIVALENTS, Size, send_message,
};
use linux_uapi::drm::DumbBuffer;

use super::{Session, invalid};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Owner {
    Desktop,
    App(u32),
}

pub(super) struct Buffer {
    pub(super) pixels: DumbBuffer,
    pub(super) size: Size,
    pub(super) owner: Owner,
    pub(super) busy: bool,
}

/// All compositor-owned client pixel buffers for the current epoch.
pub struct Buffers {
    pub(super) values: std::collections::HashMap<u32, Buffer>,
}

impl Buffers {
    pub fn get(&self, id: u32) -> Option<&DumbBuffer> {
        self.values.get(&id).map(|buffer| &buffer.pixels)
    }
}

impl Session {
    pub(super) fn allocate(&mut self, owner: Owner, request: BufferAlloc) -> io::Result<()> {
        let owner_count = self
            .buffers
            .values
            .values()
            .filter(|buffer| buffer.owner == owner)
            .count();
        let full_frame = u64::from(self.display.width) * u64::from(self.display.height) * 4;
        let owner_bytes = buffer_bytes(&self.buffers, Some(owner));
        let session_bytes = buffer_bytes(&self.buffers, None);
        let requested = u64::from(request.size.width)
            .checked_mul(u64::from(request.size.height))
            .and_then(|bytes| bytes.checked_mul(4 * u64::from(request.count)));
        let geometry_valid = match owner {
            Owner::Desktop => request.size == self.display,
            Owner::App(surface_id) => self
                .apps
                .get(&surface_id)
                .and_then(|app| app.configure)
                .is_some_and(|configure| {
                    request.size.width == configure.width * display_proto::DEVICE_SCALE_FACTOR
                        && request.size.height
                            == configure.height * display_proto::DEVICE_SCALE_FACTOR
                }),
        };
        let valid = geometry_valid
            && owner_count + request.count as usize <= 4
            && requested.is_some_and(|bytes| {
                owner_bytes + bytes <= full_frame * MAX_CONNECTION_FRAME_EQUIVALENTS
                    && session_bytes + bytes <= full_frame * MAX_SESSION_FRAME_EQUIVALENTS
            });
        if !valid {
            return self.send_allocation(
                owner,
                BufferAllocated {
                    request_id: request.request_id,
                    error: 22,
                    count: 0,
                    buffers: [BufferDescriptor::default(); 2],
                },
            );
        }
        let mut descriptors = [BufferDescriptor::default(); 2];
        for descriptor in descriptors.iter_mut().take(request.count as usize) {
            let pixels = self
                .device
                .create_dumb(request.size.width, request.size.height)?;
            let id = self.take_buffer_id()?;
            *descriptor = BufferDescriptor {
                buffer_id: id,
                gem_handle: pixels.handle().get(),
                pitch: pixels.pitch() as u32,
                byte_len: pixels.size() as u64,
            };
            self.buffers.values.insert(
                id,
                Buffer {
                    pixels,
                    size: request.size,
                    owner,
                    busy: false,
                },
            );
        }
        self.send_allocation(
            owner,
            BufferAllocated {
                request_id: request.request_id,
                error: 0,
                count: request.count,
                buffers: descriptors,
            },
        )
    }

    fn send_allocation(&self, owner: Owner, response: BufferAllocated) -> io::Result<()> {
        let stream = match owner {
            Owner::Desktop => self.desktop_stream()?,
            Owner::App(id) => {
                &self
                    .apps
                    .get(&id)
                    .ok_or_else(|| invalid("app disappeared"))?
                    .stream
            }
        };
        let mut bytes = [0u8; 96];
        let message = response
            .encode(&mut bytes)
            .ok_or_else(|| io::Error::other("allocation response encoding failed"))?;
        send_message(stream, message)
    }

    fn take_buffer_id(&mut self) -> io::Result<u32> {
        let id = self.next_buffer_id;
        self.next_buffer_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("buffer identity exhausted"))?;
        Ok(id)
    }
}

fn buffer_bytes(buffers: &Buffers, owner: Option<Owner>) -> u64 {
    buffers
        .values
        .values()
        .filter(|buffer| owner.is_none_or(|owner| buffer.owner == owner))
        .map(|buffer| buffer.pixels.size() as u64)
        .sum()
}
