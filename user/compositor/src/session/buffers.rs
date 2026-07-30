//! Compositor-owned client buffer allocation and quota accounting.

use std::io;

use display_proto::{
    BufferAlloc, BufferAllocated, BufferDescriptor, BufferRetired,
    MAX_CONNECTION_FRAME_EQUIVALENTS, MAX_SESSION_FRAME_EQUIVALENTS, Size, send_message,
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
        // Buffers sized for a superseded configure can never be presented
        // again (accept_surface checks exact geometry), so retire the idle
        // ones before accounting quota; busy ones retire on flip completion.
        if let Owner::App(surface_id) = owner {
            self.retire_stale_app_buffers(surface_id)?;
        } else {
            self.retire_stale_desktop_buffers()?;
        }
        let owner_count = self
            .buffers
            .values
            .values()
            .filter(|buffer| buffer.owner == owner)
            .count();
        // During an output transaction the last presented desktop buffer can
        // be larger than the new connector. Account in equivalents of the
        // larger live generation; otherwise shrinking the QEMU window can make
        // the old busy front alone exceed the new quota and deadlock the only
        // allocation capable of replacing it.
        let full_frame = self
            .buffers
            .values
            .values()
            .map(|buffer| u64::from(buffer.size.width) * u64::from(buffer.size.height) * 4)
            .chain(std::iter::once(
                u64::from(self.display.width) * u64::from(self.display.height) * 4,
            ))
            .max()
            .expect("display frame exists");
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
                    && within_session_quota(session_bytes, bytes, full_frame)
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

    /// Removes every idle app buffer sized for a superseded configure and
    /// tells the client to drop its mapping; busy ones retire when the
    /// compositor finishes presenting them (see `presented`).
    fn retire_stale_app_buffers(&mut self, surface_id: u32) -> io::Result<()> {
        let Some(configure) = self.apps.get(&surface_id).and_then(|app| app.configure) else {
            return Ok(());
        };
        let width = configure.width * display_proto::DEVICE_SCALE_FACTOR;
        let height = configure.height * display_proto::DEVICE_SCALE_FACTOR;
        let stale: Vec<u32> = self
            .buffers
            .values
            .iter()
            .filter(|(_, buffer)| {
                buffer.owner == Owner::App(surface_id)
                    && !buffer.busy
                    && (buffer.size.width != width || buffer.size.height != height)
            })
            .map(|(id, _)| *id)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        let stream = &self
            .apps
            .get(&surface_id)
            .ok_or_else(|| invalid("app disappeared"))?
            .stream;
        for id in stale {
            self.buffers.values.remove(&id);
            let mut bytes = [0u8; 24];
            let message = BufferRetired { buffer_id: id }
                .encode(&mut bytes)
                .ok_or_else(|| io::Error::other("release encoding failed"))?;
            send_message(stream, message)?;
        }
        Ok(())
    }

    /// Retires idle desktop buffers that cannot implement the current output
    /// serial. Busy buffers remain until the accepted scene that owns them
    /// reaches a terminal presented/discarded acknowledgement.
    pub(super) fn retire_stale_desktop_buffers(&mut self) -> io::Result<()> {
        let stale: Vec<u32> = self
            .buffers
            .values
            .iter()
            .filter(|(_, buffer)| {
                buffer.owner == Owner::Desktop && !buffer.busy && buffer.size != self.display
            })
            .map(|(id, _)| *id)
            .collect();
        let Some(desktop) = &self.desktop else {
            for id in stale {
                self.buffers.values.remove(&id);
            }
            return Ok(());
        };
        for id in stale {
            self.buffers.values.remove(&id);
            let mut bytes = [0u8; 24];
            let message = BufferRetired { buffer_id: id }
                .encode(&mut bytes)
                .ok_or_else(|| io::Error::other("release encoding failed"))?;
            send_message(&desktop.stream, message)?;
        }
        Ok(())
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

fn within_session_quota(session_bytes: u64, requested: u64, full_frame: u64) -> bool {
    session_bytes
        .checked_add(requested)
        .zip(full_frame.checked_mul(MAX_SESSION_FRAME_EQUIVALENTS))
        .is_some_and(|(total, quota)| total <= quota)
}

#[cfg(test)]
mod tests {
    use super::within_session_quota;

    #[test]
    fn session_quota_accepts_its_boundary_and_rejects_the_next_byte() {
        let frame = 20_000_000;
        assert!(within_session_quota(15 * frame, frame, frame));
        assert!(!within_session_quota(16 * frame, 1, frame));
        assert!(!within_session_quota(u64::MAX, 1, frame));
    }
}
