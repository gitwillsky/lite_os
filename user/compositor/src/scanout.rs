//! Double scanout and boot/scene composition.

mod composite;

use std::{collections::VecDeque, fs, io, thread, time::Duration};

use display_proto::{Rect, Size};
use linux_uapi::drm::{Clip, DrmDevice, DumbBuffer, FlipEvent, Topology};

use crate::{
    boot::Canvas,
    cursor::Cursor,
    session::{Buffers, Scene},
};
pub(crate) use composite::over;
use composite::{
    clear, composite_node, from_clip, group_bounds, intersect, moving_group_damage,
    source_buffer_id, to_clip, translated, union, valid_clip,
};

const EMPTY_CLIP: Clip = Clip {
    x1: 0,
    y1: 0,
    x2: 0,
    y2: 0,
};

struct Target {
    framebuffer_id: u32,
    buffer: DumbBuffer,
    revision: u64,
    cursor: Option<Rect>,
    /// Where a compositor-side move last painted the moving window group on
    /// THIS buffer, or `None` when it holds no temporary transform.
    move_paint: Option<Rect>,
}

/// Unique DRM owner with two scanout buffers.
pub struct Scanout {
    device: DrmDevice,
    topology: Topology,
    targets: [Target; 2],
    front: usize,
    logo: Vec<u8>,
    cursor: Cursor,
    history: VecDeque<(u64, Rect)>,
    prepared_damage: Rect,
}

/// Result of preparing and presenting a scene for a changed connector mode.
pub enum ModePresent {
    /// The scene reached the new scanout through a real page-flip completion.
    Presented(FlipEvent),
    /// The connector changed again before the requested mode could be latched.
    Superseded(Size),
}

impl Scanout {
    /// Reports whether the platform published a usable DRM display topology.
    pub fn available() -> bool {
        DrmDevice::open("/dev/dri/card0")
            .and_then(|device| device.query_topology())
            .is_ok()
    }

    /// Opens DRM, takes master, allocates the pair and immediately publishes the boot scene.
    pub fn open() -> io::Result<Self> {
        let device = DrmDevice::open("/dev/dri/card0")?;
        let topology = device.query_topology()?;
        let mut attempts = 0;
        loop {
            match device.set_master() {
                Ok(()) => break,
                Err(error) if error.raw_os_error() == Some(16) && attempts < 50 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        }
        let width = u32::from(topology.mode.width());
        let height = u32::from(topology.mode.height());
        let first = Self::target(&device, width, height)?;
        let second = Self::target(&device, width, height)?;
        let mut scanout = Self {
            device,
            topology,
            targets: [first, second],
            front: 0,
            logo: fs::read("/usr/share/liteos/bootlogo.xrgb").unwrap_or_default(),
            cursor: Cursor::open()?,
            history: VecDeque::new(),
            prepared_damage: Rect::default(),
        };
        scanout.draw_boot(0);
        scanout.draw_boot(1);
        scanout
            .device
            .set_crtc(&scanout.topology, scanout.targets[0].framebuffer_id)?;
        eprintln!("compositor: mode {}x{}", width, height);
        Ok(scanout)
    }

    fn target(device: &DrmDevice, width: u32, height: u32) -> io::Result<Target> {
        let buffer = device.create_dumb(width, height)?;
        let framebuffer_id = device.add_framebuffer(&buffer, 24)?;
        Ok(Target {
            framebuffer_id,
            buffer,
            revision: 0,
            cursor: None,
            move_paint: None,
        })
    }

    /// Returns the shared DRM file-description owner.
    pub fn device(&self) -> &DrmDevice {
        &self.device
    }

    /// Returns the physical mode.
    pub fn size(&self) -> Size {
        Size {
            width: u32::from(self.topology.mode.width()),
            height: u32::from(self.topology.mode.height()),
        }
    }

    /// Rebuilds both scanout targets and atomically presents a scene at the
    /// exact connector generation it was rendered for.
    pub fn present_mode(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        cursor: (i32, i32),
    ) -> io::Result<ModePresent> {
        let topology = self.device.query_topology()?;
        let actual = topology_size(&topology);
        if actual != scene.output_size {
            return Ok(ModePresent::Superseded(actual));
        }
        let mut next = [
            Self::target(&self.device, actual.width, actual.height)?,
            Self::target(&self.device, actual.width, actual.height)?,
        ];
        let screen = Rect {
            x: 0,
            y: 0,
            width: actual.width,
            height: actual.height,
        };
        for target in &mut next {
            clear(&mut target.buffer, screen);
            for node in &scene.nodes {
                let source = buffers.get(node.buffer_id).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "scene buffer disappeared")
                })?;
                composite_node(&mut target.buffer, source, node, screen, screen, (0, 0));
            }
            target.revision = scene.revision;
        }
        let cursor_clip = self.cursor.overlay(&mut next[1].buffer, cursor.0, cursor.1);
        next[1].cursor = from_clip(cursor_clip);
        self.device.dirty(next[1].framebuffer_id, &[])?;
        if let Err(error) = self.device.set_crtc(&topology, next[0].framebuffer_id) {
            Self::remove_targets(&self.device, &next);
            let latest = self.device.query_topology()?;
            let latest_size = topology_size(&latest);
            if latest_size != actual {
                return Ok(ModePresent::Superseded(latest_size));
            }
            return Err(error);
        }
        if let Err(error) = self
            .device
            .page_flip(&topology, next[1].framebuffer_id, scene.revision)
        {
            Self::remove_targets(&self.device, &next);
            return Err(error);
        }
        let event = self.device.read_flip_event()?;
        if event.user_data != scene.revision {
            Self::remove_targets(&self.device, &next);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mode page-flip sequence mismatch",
            ));
        }
        let old = std::mem::replace(&mut self.targets, next);
        Self::remove_targets(&self.device, &old);
        self.topology = topology;
        self.front = 1;
        self.history.clear();
        self.prepared_damage = screen;
        eprintln!("compositor: mode {}x{}", actual.width, actual.height);
        Ok(ModePresent::Presented(event))
    }

    fn remove_targets(device: &DrmDevice, targets: &[Target; 2]) {
        for target in targets {
            let _ = device.remove_framebuffer(target.framebuffer_id);
        }
    }

    /// Returns scanout to the same clean state [`Self::open`] leaves behind.
    ///
    /// A desktop disconnect resets the session epoch (dropping every client
    /// buffer and the presented scene), but scanout state is separate and would
    /// otherwise persist across the boundary: both targets keep their last scene
    /// `revision`, the damage `history` still references retired revisions, and
    /// `prepared_damage` describes the old scene. On the next boot the
    /// revision-based damage diff in [`Self::compose`] (`revision == 0` means
    /// full-screen) would then paint only a stale sub-rectangle over the last
    /// desktop pixels — the cold-start ghosting. Repainting boot into both
    /// buffers and forcing both revisions to 0 restores the guarantee that the
    /// first post-reset compose is a full-screen paint, exactly as after
    /// `open()`. `front` is left untouched and its framebuffer re-scanned so the
    /// display never shows a torn intermediate frame.
    pub fn reset_to_boot(&mut self) -> io::Result<()> {
        let topology = self.device.query_topology()?;
        if topology_size(&topology) != self.size() {
            let size = topology_size(&topology);
            let next = [
                Self::target(&self.device, size.width, size.height)?,
                Self::target(&self.device, size.width, size.height)?,
            ];
            let old = std::mem::replace(&mut self.targets, next);
            self.topology = topology;
            self.front = 0;
            Self::remove_targets(&self.device, &old);
        }
        self.draw_boot(0);
        self.draw_boot(1);
        for target in &mut self.targets {
            target.revision = 0;
            target.cursor = None;
            target.move_paint = None;
        }
        // A dead epoch may have left a non-arrow shape selected; the next
        // desktop's first Motion re-establishes pointer focus, but the arrow is
        // the correct default until then.
        self.cursor.set_shape(display_proto::CURSOR_DEFAULT);
        self.history.clear();
        self.prepared_damage = Rect::default();
        self.device
            .set_crtc(&self.topology, self.targets[self.front].framebuffer_id)
    }

    fn draw_boot(&mut self, target: usize) {
        let buffer = &mut self.targets[target].buffer;
        // SAFETY: DumbBuffer owns a writable pitch*height mapping for the Canvas lifetime.
        let mut canvas = unsafe {
            Canvas::new(
                buffer.as_mut_ptr(),
                buffer.pitch(),
                buffer.width(),
                buffer.height(),
            )
        };
        canvas.draw_background();
        canvas.draw_bootlogo(&self.logo);
    }

    /// Composes the accepted flat scene into the back scanout, without the cursor.
    ///
    /// The cursor is applied separately by [`Self::present`] so that pointer motion
    /// can be served by [`Self::move_cursor`] without recompositing the scene.
    pub fn compose(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
    ) -> io::Result<()> {
        let back = 1 - self.front;
        let screen = Rect {
            x: 0,
            y: 0,
            width: self.targets[back].buffer.width() as u32,
            height: self.targets[back].buffer.height() as u32,
        };
        let mut damage = if self.targets[back].revision == 0 {
            screen
        } else {
            self.history
                .iter()
                .filter(|(revision, _)| *revision > self.targets[back].revision)
                .map(|(_, damage)| *damage)
                .fold(scene.damage, union)
        };
        if let Some(cursor) = self.targets[back].cursor {
            damage = union(damage, cursor);
        }
        // The moving group's damage must cover every position this back buffer
        // can show it at: the canonical scene bounds, the CURRENT temporary
        // offset, and — via the per-target `move_paint` record — the offset a
        // previous paint left on THIS buffer. Without the stale rect, a scene
        // concurrently submitted mid-grab (e.g. the music player's playback
        // commits) flips with the previous temporary position still painted,
        // and no later damage ever covers it: the fast-drag trails.
        let (group, offset) = active_move
            .map(|(group, offset, _)| (Some(group), offset))
            .unwrap_or((None, (0, 0)));
        let canonical = group.and_then(|group| group_bounds(&scene.nodes, group));
        let move_bounds = canonical.map(|bounds| translated(bounds, offset));
        let stale = self.targets[back].move_paint.take();
        if let Some(extra) = moving_group_damage(canonical, offset, stale) {
            damage = union(damage, extra);
        }
        damage = intersect(damage, screen).unwrap_or(screen);
        let target = &mut self.targets[back].buffer;
        clear(target, damage);
        for node in &scene.nodes {
            let source_id = source_buffer_id(node, active_move);
            let Some(source) = buffers.get(source_id) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "scene buffer disappeared",
                ));
            };
            let offset = active_move
                .filter(|(window_group, _, _)| node.window_group == *window_group)
                .map_or((0, 0), |(_, offset, _)| offset);
            composite_node(target, source, node, screen, damage, offset);
        }
        self.targets[back].revision = scene.revision;
        self.targets[back].cursor = None;
        // Record where this paint left the moving group on the back buffer, so
        // the next compose on it cleans that position even after page flips
        // (a flip makes the other buffer stale in exactly the same way).
        self.targets[back].move_paint = move_bounds;
        self.prepared_damage = damage;
        self.history.push_back((scene.revision, scene.damage));
        let oldest = self
            .targets
            .iter()
            .filter(|target| target.revision != 0)
            .map(|target| target.revision)
            .min()
            .unwrap_or(scene.revision);
        while self
            .history
            .front()
            .is_some_and(|(revision, _)| *revision <= oldest)
        {
            self.history.pop_front();
        }
        Ok(())
    }

    /// Repaints only the old/new window-group union on the current front buffer.
    ///
    /// The canonical scene remains unchanged; `offset` is a compositor-owned
    /// temporary transform authorized by the desktop. Painting every scene node
    /// inside the damage rectangle restores occlusion correctly without a
    /// full-screen React render, scanout compose, or page flip.
    pub fn compose_move(
        &mut self,
        nodes: &[crate::session::Node],
        buffers: &Buffers,
        active_move: (u32, (i32, i32), u32),
        damage: Rect,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        let (window_group, offset, _) = active_move;
        let front = self.front;
        let screen = Rect {
            x: 0,
            y: 0,
            width: self.targets[front].buffer.width() as u32,
            height: self.targets[front].buffer.height() as u32,
        };
        let Some(damage) = intersect(damage, screen) else {
            return Ok(());
        };
        let target = &mut self.targets[front].buffer;
        let old_cursor = self.cursor.remove(target);
        clear(target, damage);
        for node in nodes {
            let source_id = source_buffer_id(node, Some(active_move));
            let source = buffers.get(source_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "scene buffer disappeared")
            })?;
            composite_node(
                target,
                source,
                node,
                screen,
                damage,
                if node.window_group == window_group {
                    offset
                } else {
                    (0, 0)
                },
            );
        }
        let new_cursor = self.cursor.overlay(target, cursor.0, cursor.1);
        self.targets[front].cursor = from_clip(new_cursor);
        // The moving group is now painted at `offset` on the front buffer; a
        // later flip turns this buffer into the back, whose next full compose
        // must still clean this exact position.
        self.targets[front].move_paint =
            group_bounds(nodes, window_group).map(|bounds| translated(bounds, offset));
        let mut clips = [to_clip(damage), EMPTY_CLIP, EMPTY_CLIP];
        let mut clip_count = 1;
        for cursor in [old_cursor, new_cursor] {
            if valid_clip(&cursor) {
                clips[clip_count] = cursor;
                clip_count += 1;
            }
        }
        self.device
            .dirty(self.targets[front].framebuffer_id, &clips[..clip_count])
    }

    /// Overlays the cursor into the freshly composed back buffer and flips.
    ///
    /// 1. Rasterizes the cursor into the back buffer, saving the clean scene pixels
    ///    beneath it so a later [`Self::move_cursor`] can erase it in place.
    /// 2. Queues and waits for one exact page-flip completion.
    ///
    /// After the flip the back buffer becomes the front, so the cursor backing store
    /// consistently describes the scanned-out buffer for subsequent motion damage.
    pub fn present_scene(&mut self, revision: u64, cursor: (i32, i32)) -> io::Result<FlipEvent> {
        let back = 1 - self.front;
        let cursor_clip = self
            .cursor
            .overlay(&mut self.targets[back].buffer, cursor.0, cursor.1);
        // The kernel cannot observe CPU writes to dumb buffers: a framebuffer's
        // host resource is only refreshed by `DRM_IOCTL_MODE_DIRTYFB`, and the
        // flip skips the transfer entirely once any earlier dirty marked it
        // synchronized. Sync the freshly composed back buffer first — empty
        // clips mean the full framebuffer — or the flip presents stale pixels
        // (frozen scenes and cursor remnants baked into old frames).
        let damage = from_clip(cursor_clip).map_or(self.prepared_damage, |cursor| {
            union(self.prepared_damage, cursor)
        });
        self.targets[back].cursor = from_clip(cursor_clip);
        self.device
            .dirty(self.targets[back].framebuffer_id, &[to_clip(damage)])?;
        self.present(revision)
    }

    /// Serves pointer motion by relocating the cursor on the scanned-out buffer and
    /// flushing only the damaged rectangles, avoiding a recompose and page flip.
    ///
    /// 1. Restores the clean pixels under the old cursor and paints the new one on
    ///    the current front buffer.
    /// 2. Reports the union of old and new cursor boxes through `DRM_IOCTL_MODE_DIRTYFB`.
    ///
    /// Empty clips (cursor fully off-screen) are dropped; an all-empty update is a no-op.
    pub fn move_cursor(&mut self, cursor: (i32, i32)) -> io::Result<()> {
        let front = self.front;
        let damage = self
            .cursor
            .relocate(&mut self.targets[front].buffer, cursor.0, cursor.1);
        let mut clips = [EMPTY_CLIP; 2];
        let mut clip_count = 0;
        for clip in damage {
            if valid_clip(&clip) {
                clips[clip_count] = clip;
                clip_count += 1;
            }
        }
        if clip_count == 0 {
            return Ok(());
        }
        self.targets[front].cursor = from_clip(clips[clip_count - 1]);
        self.device
            .dirty(self.targets[front].framebuffer_id, &clips[..clip_count])
    }

    /// Selects the standard cursor shape and, when it changes, repaints the
    /// cursor in place on the scanned-out front buffer so the new shape appears
    /// immediately without recomposing or page-flipping.
    ///
    /// A shape that is already active is a no-op. Reuses the same relocate +
    /// `DIRTYFB` path as [`Self::move_cursor`], relocating to the current
    /// position so the old shape is erased and the new one drawn.
    pub fn set_cursor_shape(&mut self, shape: u32, cursor: (i32, i32)) -> io::Result<()> {
        if !self.cursor.set_shape(shape) {
            return Ok(());
        }
        self.move_cursor(cursor)
    }

    /// Queues and waits for one exact page-flip completion.
    pub fn present(&mut self, revision: u64) -> io::Result<FlipEvent> {
        let back = 1 - self.front;
        self.device
            .page_flip(&self.topology, self.targets[back].framebuffer_id, revision)?;
        let event = self.device.read_flip_event()?;
        if event.user_data != revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "page-flip sequence mismatch",
            ));
        }
        self.front = back;
        Ok(event)
    }
}

impl Drop for Scanout {
    fn drop(&mut self) {
        Self::remove_targets(&self.device, &self.targets);
    }
}

fn topology_size(topology: &Topology) -> Size {
    Size {
        width: u32::from(topology.mode.width()),
        height: u32::from(topology.mode.height()),
    }
}
#[cfg(test)]
mod tests;
