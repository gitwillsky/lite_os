//! Double scanout and boot/scene composition.

use std::{collections::VecDeque, fs, io, thread, time::Duration};

use display_proto::{Rect, SceneNodeKind, Size};
use linux_uapi::drm::{Clip, DrmDevice, DumbBuffer, FlipEvent, Topology};

use crate::{
    boot::Canvas,
    cursor::Cursor,
    session::{Buffers, Scene},
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
        scanout.draw_boot(0, 0);
        scanout.draw_boot(1, 0);
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

    /// Draws one real 30 Hz boot animation frame into the back target.
    pub fn render_boot(&mut self, offset: usize) -> io::Result<()> {
        self.draw_boot(1 - self.front, offset);
        Ok(())
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
        self.draw_boot(0, 0);
        self.draw_boot(1, 0);
        for target in &mut self.targets {
            target.revision = 0;
            target.cursor = None;
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

    fn draw_boot(&mut self, target: usize, offset: usize) {
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
        canvas.fill(0);
        canvas.draw_bootlogo(&self.logo);
        let origin = canvas.track_origin();
        canvas.draw_track(origin.0, origin.1);
        canvas.draw_sliders(origin.0, origin.1, offset);
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
        if let Some((window_group, offset, _)) = active_move
            && let Some(bounds) = group_bounds(&scene.nodes, window_group)
        {
            // A direct front-buffer move is not represented by scene revisions.
            // Repainting both positions makes a concurrently submitted scene
            // inherit the temporary transform instead of snapping back.
            damage = union(damage, union(bounds, translated(bounds, offset)));
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
        for target in &self.targets {
            let _ = self.device.remove_framebuffer(target.framebuffer_id);
        }
    }
}

fn composite_node(
    target: &mut DumbBuffer,
    source: &DumbBuffer,
    node: &crate::session::Node,
    screen: Rect,
    damage: Rect,
    offset: (i32, i32),
) {
    let bounds = translated(node.bounds, offset);
    let clip = translated(node.clip, offset);
    let x1 = bounds.x.max(clip.x).max(screen.x).max(0);
    let x1 = x1.max(damage.x);
    let y1 = bounds.y.max(clip.y).max(screen.y).max(0).max(damage.y);
    let x2 = (bounds.x + bounds.width as i32)
        .min(clip.x + clip.width as i32)
        .min(screen.width as i32)
        .min(damage.x.saturating_add_unsigned(damage.width));
    let y2 = (bounds.y + bounds.height as i32)
        .min(clip.y + clip.height as i32)
        .min(screen.height as i32)
        .min(damage.y.saturating_add_unsigned(damage.height));
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    // Rounded corners: rows within `corner_radius` of the clip top inset
    // horizontally so the frame clip skips the top corner cutout, letting
    // lower content show through instead of being covered by stale chrome
    // pixels. Chrome and windows are both `8px 8px 0 0` (top-only), so only
    // the top edge rounds; the bottom stays square. The inset math mirrors the
    // renderer's `corner_inset` so the clip edge aligns with the painted arc.
    let r = node.corner_radius as f32;
    let r_sq = r * r;
    for y in y1..y2 {
        let source_y = (y - bounds.y) as usize;
        let source_row = source.row(source_y);
        let target_row = target.row_mut(y as usize);
        let (mut px1, mut px2) = (x1, x2);
        if node.corner_radius > 0 {
            // Distance in rows from the top clip edge; only rows inside the top
            // corner region get inset.
            let edge_dist = y - clip.y;
            if edge_dist >= 0 && (edge_dist as f32) < r {
                let mid = edge_dist as f32 + 0.5;
                let dist = r - mid;
                let inset = r - (r_sq - dist * dist).max(0.0).sqrt();
                let left_px = (clip.x as f32 + inset).ceil() as i32;
                if px1 < left_px {
                    px1 = left_px;
                }
                let right_px = (clip.x as f32 + clip.width as f32 - inset).floor() as i32;
                if px2 > right_px {
                    px2 = right_px;
                }
            }
        }
        if px2 <= px1 {
            continue;
        }
        let opaque = node.opaque.map(|rectangle| translated(rectangle, offset));
        if opaque.is_some_and(|opaque| {
            y >= opaque.y
                && y < opaque.y.saturating_add_unsigned(opaque.height)
                && px1 >= opaque.x
                && px2 <= opaque.x.saturating_add_unsigned(opaque.width)
        }) {
            let source_start = (px1 - bounds.x) as usize;
            let source_end = (px2 - bounds.x) as usize;
            target_row[px1 as usize..px2 as usize]
                .copy_from_slice(&source_row[source_start..source_end]);
            continue;
        }
        for x in px1..px2 {
            let source_pixel = source_row[(x - bounds.x) as usize];
            target_row[x as usize] = over(source_pixel, target_row[x as usize]);
        }
    }
}

fn clear(target: &mut DumbBuffer, rectangle: Rect) {
    let x1 = rectangle.x as usize;
    let x2 = x1 + rectangle.width as usize;
    for y in rectangle.y as usize..rectangle.y as usize + rectangle.height as usize {
        target.row_mut(y)[x1..x2].fill(0);
    }
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
}

fn group_bounds(nodes: &[crate::session::Node], window_group: u32) -> Option<Rect> {
    nodes
        .iter()
        .filter(|node| node.window_group == window_group)
        .filter_map(|node| intersect(node.bounds, node.clip))
        .reduce(union)
}

fn source_buffer_id(
    node: &crate::session::Node,
    active_move: Option<(u32, (i32, i32), u32)>,
) -> u32 {
    active_move.map_or(node.buffer_id, |(window_group, _, underlay)| {
        if node.kind == SceneNodeKind::Pixels && node.window_group != window_group {
            underlay
        } else {
            node.buffer_id
        }
    })
}

fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .min(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .min(right.y.saturating_add_unsigned(right.height));
    (x2 > x1 && y2 > y1).then_some(Rect {
        x: x1,
        y: y1,
        width: (x2 - x1) as u32,
        height: (y2 - y1) as u32,
    })
}

fn union(left: Rect, right: Rect) -> Rect {
    let x1 = left.x.min(right.x);
    let y1 = left.y.min(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .max(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .max(right.y.saturating_add_unsigned(right.height));
    Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1) as u32,
        height: y2.saturating_sub(y1) as u32,
    }
}

fn to_clip(rectangle: Rect) -> Clip {
    Clip {
        x1: rectangle.x as u16,
        y1: rectangle.y as u16,
        x2: rectangle.x.saturating_add_unsigned(rectangle.width) as u16,
        y2: rectangle.y.saturating_add_unsigned(rectangle.height) as u16,
    }
}

fn valid_clip(clip: &Clip) -> bool {
    clip.x2 > clip.x1 && clip.y2 > clip.y1
}

fn from_clip(clip: Clip) -> Option<Rect> {
    valid_clip(&clip).then_some(Rect {
        x: i32::from(clip.x1),
        y: i32::from(clip.y1),
        width: u32::from(clip.x2 - clip.x1),
        height: u32::from(clip.y2 - clip.y1),
    })
}

/// Composites one premultiplied ARGB8888 `source` pixel over `destination`.
///
/// Porter-Duff OVER for premultiplied source: `out = source + dest * (1 - a)`.
/// The `source` color channels must already be scaled by its alpha — straight
/// alpha would double-count the coverage and render translucent edges too bright.
/// The result carries no alpha (the scanout buffer is presented as XRGB8888).
///
/// Shared with the cursor overlay ([`crate::cursor`]), which alpha-blends its
/// RGBA shape pixels through the same operator so rounding stays identical.
pub(crate) fn over(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 255 {
        return source & 0x00ff_ffff;
    }
    if alpha == 0 {
        return destination;
    }
    let inverse = 255 - alpha;
    let red = ((source >> 16) & 0xff) + (((destination >> 16) & 0xff) * inverse + 127) / 255;
    let green = ((source >> 8) & 0xff) + (((destination >> 8) & 0xff) * inverse + 127) / 255;
    let blue = (source & 0xff) + ((destination & 0xff) * inverse + 127) / 255;
    (red.min(255) << 16) | (green.min(255) << 8) | blue.min(255)
}

#[cfg(test)]
mod tests;
