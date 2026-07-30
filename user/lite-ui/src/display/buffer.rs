//! Client mapping ownership and compositor-driven buffer lifecycle.

use std::io;

use display_proto::Size;
use linux_uapi::drm::SharedDumbBuffer;

use super::{Display, invalid};

pub(super) struct Buffer {
    pub(super) id: u32,
    pub(super) pixels: SharedDumbBuffer,
    pub(super) free: bool,
}

impl Buffer {
    /// Returns whether this mapping belongs to the active physical geometry.
    pub(super) fn matches(&self, physical: Size) -> bool {
        self.pixels.width() == physical.width as usize
            && self.pixels.height() == physical.height as usize
    }
}

impl Display {
    /// Makes one current-generation mapping writable again.
    pub(super) fn release(&mut self, id: u32) -> io::Result<()> {
        let Some(index) = self.buffers.iter().position(|buffer| buffer.id == id) else {
            return Err(invalid("unknown buffer released"));
        };
        let buffer = &mut self.buffers[index];
        if buffer.free {
            return Err(invalid("buffer released twice"));
        }
        buffer.free = true;
        Ok(())
    }

    /// Permanently removes one mapping from an obsolete geometry generation.
    pub(super) fn retire(&mut self, id: u32) -> io::Result<()> {
        let index = self
            .buffers
            .iter()
            .position(|buffer| buffer.id == id)
            .ok_or_else(|| invalid("unknown buffer retired"))?;
        self.buffers.remove(index);
        Ok(())
    }
}
