//! Compositor-private GPU render-target ownership.

use std::{collections::HashMap, io};

use display_proto::Size;
use linux_uapi::drm::VirglResource;

use super::Session;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum Owner {
    Desktop,
    App(u32),
}

pub(super) struct Buffer {
    pub(super) pixels: VirglResource,
    pub(super) size: Size,
    pub(super) owner: Owner,
    pub(super) busy: bool,
}

/// GPU targets referenced by the current and pending flat scenes.
pub struct Buffers {
    pub(super) values: HashMap<u32, Buffer>,
}

impl Buffers {
    pub fn get(&self, id: u32) -> Option<&VirglResource> {
        self.values.get(&id).map(|buffer| &buffer.pixels)
    }
}

impl Session {
    pub(super) fn take_buffer_id(&mut self) -> io::Result<u32> {
        let id = self.next_buffer_id;
        self.next_buffer_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("GPU target identity exhausted"))?;
        Ok(id)
    }

    /// Drops idle targets from an obsolete output geometry.
    pub(super) fn retire_stale_desktop_buffers(&mut self) -> io::Result<()> {
        self.buffers.values.retain(|_, buffer| {
            buffer.owner != Owner::Desktop || buffer.busy || buffer.size == self.display
        });
        self.move_underlays
            .retain(|_, id| self.buffers.values.contains_key(id));
        if self
            .desktop_render_id
            .is_some_and(|id| !self.buffers.values.contains_key(&id))
        {
            self.desktop_render_id = None;
        }
        Ok(())
    }
}
