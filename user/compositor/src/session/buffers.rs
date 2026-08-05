//! Compositor-private GPU render-target ownership.

use std::{collections::HashMap, io};

use display_proto::{Rect, Size};
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
    pub(super) revision: u64,
    /// Bounding region whose pixels differ from the owner's latest published
    /// paint revision. Preserving this with a retired target lets the next
    /// retained paint repair only stale pixels; dropping it forces a full-screen
    /// base copy on every scroll or resize frame.
    pub(super) repair: Option<Rect>,
}

/// One retired half of an owner's GPU paint double buffer.
pub(crate) struct PaintTarget {
    /// Reusable compositor-owned VirGL storage.
    pub(crate) pixels: VirglResource,
    /// Exact bounding repair needed to match the latest published paint base.
    pub(crate) repair: Option<Rect>,
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
        if self
            .desktop_render_id
            .is_some_and(|id| !self.buffers.values.contains_key(&id))
        {
            self.desktop_render_id = None;
        }
        self.idle_targets.remove(&Owner::Desktop);
        Ok(())
    }
}

pub(super) fn extend_repair(current: Option<Rect>, changed: Rect) -> Option<Rect> {
    if changed.width == 0 || changed.height == 0 {
        return current;
    }
    let Some(current) = current else {
        return Some(changed);
    };
    let x1 = current.x.min(changed.x);
    let y1 = current.y.min(changed.y);
    let x2 = current
        .x
        .saturating_add_unsigned(current.width)
        .max(changed.x.saturating_add_unsigned(changed.width));
    let y2 = current
        .y
        .saturating_add_unsigned(current.height)
        .max(changed.y.saturating_add_unsigned(changed.height));
    Some(Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1) as u32,
        height: y2.saturating_sub(y1) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::extend_repair;
    use display_proto::Rect;

    #[test]
    fn retired_target_accumulates_only_changed_pixel_bounds() {
        let repair = extend_repair(
            Some(Rect {
                x: 100,
                y: 200,
                width: 40,
                height: 50,
            }),
            Rect {
                x: 120,
                y: 180,
                width: 80,
                height: 30,
            },
        );
        assert_eq!(
            repair,
            Some(Rect {
                x: 100,
                y: 180,
                width: 100,
                height: 70,
            })
        );
        assert_eq!(repair, extend_repair(repair, Rect::default()));
    }
}
