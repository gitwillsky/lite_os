//! GPU double scanout and atomic scene presentation.

use std::{fs, io, thread, time::Duration};

use display_proto::{DisplayListCommit, Rect, Size, TextureFormat, TextureRect};
use linux_uapi::drm::{DrmDevice, FlipEvent, Topology, VirglContext, VirglResource};

use crate::{
    boot::Canvas,
    cursor::Cursor,
    gpu::{GpuRenderer, TextureLayer, intersect},
    session::{Buffers, Node, Scene},
};

struct Target {
    framebuffer_id: u32,
    buffer: VirglResource,
    revision: u64,
    // Records the temporary transform already rasterized into this target.
    // Without target-local ownership, alternating scanouts cannot derive the
    // old damage region and must repaint the full 3008×1692 output every move.
    move_state: Option<(u32, (i32, i32))>,
    // Region where this target differs from the latest canonical scene. The
    // alternating scanout target carries it until reuse; without this owner a
    // partial scene would either inherit pixels two revisions old or repaint
    // the complete output every frame.
    repair: Option<Rect>,
}

#[derive(Default)]
struct MoveFrames {
    in_flight: Option<u64>,
    latest_pending: bool,
}

impl MoveFrames {
    fn request(&mut self) -> bool {
        if self.in_flight.is_some() {
            // Pointer position/shape and move transform live in their existing
            // owners. This bit only records that the frame currently in flight
            // no longer contains their latest values; without it the last event
            // in a burst can be stranded forever after evdev becomes idle.
            self.latest_pending = true;
            false
        } else {
            true
        }
    }

    fn submitted(&mut self, revision: u64) {
        assert!(self.in_flight.replace(revision).is_none());
    }

    fn completed(&mut self, revision: u64) -> io::Result<bool> {
        if self.in_flight != Some(revision) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "move page-flip sequence mismatch",
            ));
        }
        self.in_flight = None;
        Ok(std::mem::take(&mut self.latest_pending))
    }

    fn is_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    fn reset(&mut self) {
        assert!(self.in_flight.is_none());
        self.latest_pending = false;
    }
}

/// Unique DRM and VirGL owner with two GPU scanout targets.
pub struct Scanout {
    device: DrmDevice,
    graphics: VirglContext,
    renderer: GpuRenderer,
    topology: Topology,
    targets: [Target; 2],
    front: usize,
    logo: Vec<u8>,
    cursor: Cursor,
    cursor_published: bool,
    move_frames: MoveFrames,
}

/// Result of presenting a scene for a changed connector mode.
pub enum ModePresent {
    /// The scene reached the new scanout through a real page-flip completion.
    Presented(FlipEvent),
    /// The connector changed again before the requested mode could be latched.
    Superseded(Size),
}

impl Scanout {
    /// Reports whether the platform publishes usable KMS topology and VirGL2.
    pub fn available() -> bool {
        DrmDevice::open("/dev/dri/card0")
            .and_then(|device| device.query_topology())
            .is_ok()
    }

    /// Opens DRM, creates the sole GPU context, and publishes the boot image.
    pub fn open() -> io::Result<Self> {
        let device = DrmDevice::open("/dev/dri/card0")?;
        let topology = device.query_topology()?;
        let graphics = device.initialize_virgl("liteos-compositor")?;
        let renderer = GpuRenderer::new(&graphics)?;
        let cursor = Cursor::open(&device)?;
        take_master(&device)?;
        let width = u32::from(topology.mode.width());
        let height = u32::from(topology.mode.height());
        let targets = [
            Self::target(&device, &graphics, width, height)?,
            Self::target(&device, &graphics, width, height)?,
        ];
        let mut scanout = Self {
            device,
            graphics,
            renderer,
            topology,
            targets,
            front: 0,
            logo: fs::read("/usr/share/liteos/bootlogo.xrgb").unwrap_or_default(),
            cursor,
            cursor_published: false,
            move_frames: MoveFrames::default(),
        };
        scanout.draw_boot(0);
        scanout.draw_boot(1);
        scanout.upload_boot(0)?;
        scanout.upload_boot(1)?;
        scanout
            .device
            .set_crtc(&scanout.topology, scanout.targets[0].framebuffer_id)?;
        scanout
            .device
            .update_cursor(&scanout.topology, None, (0, 0), (0, 0))?;
        eprintln!("compositor: GPU mode {width}x{height}");
        Ok(scanout)
    }

    fn target(
        device: &DrmDevice,
        graphics: &VirglContext,
        width: u32,
        height: u32,
    ) -> io::Result<Target> {
        let buffer = graphics.create_render_target(width, height)?;
        let framebuffer_id = device.add_virgl_framebuffer(&buffer, 24)?;
        Ok(Target {
            framebuffer_id,
            buffer,
            revision: 0,
            move_state: None,
            repair: None,
        })
    }

    /// Returns the shared DRM file-description owner.
    pub fn device(&self) -> &DrmDevice {
        &self.device
    }

    /// Returns the compositor's single VirGL context.
    pub fn graphics(&self) -> &VirglContext {
        &self.graphics
    }

    /// Returns the physical connector mode.
    pub fn size(&self) -> Size {
        topology_size(&self.topology)
    }

    /// Creates one compositor-owned GPU paint target.
    pub fn create_paint_target(&self, size: Size) -> io::Result<VirglResource> {
        self.graphics.create_texture(size.width, size.height)
    }

    /// Rasterizes one client display list into its reusable GPU target.
    pub fn render_display_list<'a>(
        &'a self,
        target: VirglResource,
        list: DisplayListCommit<'_>,
        base: Option<&VirglResource>,
        repair: Option<Rect>,
        cache_owner: (u64, u32),
        texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
    ) -> io::Result<VirglResource> {
        self.renderer
            .render_display_list(&target, list, base, repair, cache_owner, texture)?;
        // Paint and scene composition share one VirGL context, so a later
        // sampling submit observes every earlier target write in context order.
        // Blocking this publication on the CPU would stall evdev/socket polling
        // for the full GPU fence latency on every keypress.
        Ok(target)
    }

    /// Rasterizes a desktop underlay while omitting one movable window group.
    pub fn render_display_list_excluding<'a>(
        &'a self,
        size: Size,
        list: DisplayListCommit<'_>,
        texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
        group: u32,
    ) -> io::Result<VirglResource> {
        let target = self.graphics.create_texture(size.width, size.height)?;
        self.renderer
            .render_display_list_excluding(&target, list, texture, group)?;
        Ok(target)
    }

    /// Flattens every non-moving scene node over the desktop raster captured at
    /// move authorization time.
    pub fn compose_move_underlay(
        &self,
        desktop: VirglResource,
        nodes: &[Node],
        buffers: &Buffers,
        moving_group: u32,
    ) -> io::Result<VirglResource> {
        let target = self
            .graphics
            .create_texture(desktop.width(), desktop.height())?;
        let mut layers = Vec::with_capacity(nodes.len());
        for node in nodes
            .iter()
            .filter(|node| node.window_group != moving_group)
        {
            let texture = if node.kind == display_proto::SceneNodeKind::DisplayList {
                &desktop
            } else {
                buffers
                    .get(node.buffer_id)
                    .ok_or_else(scene_buffer_disappeared)?
            };
            layers.push(scene_layer(texture, node, (0, 0), None));
        }
        self.renderer.render(&target, &layers)?;
        Ok(target)
    }

    /// Rebuilds both targets and atomically presents at a new connector mode.
    pub fn present_mode(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        cursor: (i32, i32),
    ) -> io::Result<ModePresent> {
        let actual = scene.output_size;
        let topology = self.topology.with_size(actual.width, actual.height)?;
        let mut next = [
            Self::target(&self.device, &self.graphics, actual.width, actual.height)?,
            Self::target(&self.device, &self.graphics, actual.width, actual.height)?,
        ];
        self.render_target(&next[0].buffer, &scene.nodes, buffers, None, None)?;
        self.render_target(&next[1].buffer, &scene.nodes, buffers, None, None)?;
        next[0].revision = scene.revision;
        next[1].revision = scene.revision;
        next[0].move_state = None;
        next[1].move_state = None;
        next[0].repair = None;
        next[1].repair = None;
        if let Err(error) = self.device.set_crtc(&topology, next[0].framebuffer_id) {
            Self::remove_targets(&self.device, &next);
            let latest_size = topology_size(&self.device.query_topology()?);
            if latest_size != self.size() && latest_size != actual {
                return Ok(ModePresent::Superseded(latest_size));
            }
            return Err(error);
        }
        self.device
            .page_flip(&topology, next[1].framebuffer_id, scene.revision)?;
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
        self.cursor.present(&self.device, &self.topology, cursor)?;
        self.cursor_published = true;
        eprintln!("compositor: GPU mode {}x{}", actual.width, actual.height);
        Ok(ModePresent::Presented(event))
    }

    /// Restores the pre-desktop boot image after an epoch reset.
    pub fn reset_to_boot(&mut self) -> io::Result<()> {
        self.move_frames.reset();
        self.draw_boot(0);
        self.draw_boot(1);
        self.upload_boot(0)?;
        self.upload_boot(1)?;
        for target in &mut self.targets {
            target.revision = 0;
            target.move_state = None;
            target.repair = None;
        }
        self.cursor.set_shape(display_proto::CURSOR_DEFAULT);
        self.device
            .update_cursor(&self.topology, None, (0, 0), (0, 0))?;
        self.cursor_published = false;
        self.device
            .set_crtc(&self.topology, self.targets[self.front].framebuffer_id)
    }

    /// Uploads changed client textures and renders the accepted scene on the GPU.
    pub fn compose(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
    ) -> io::Result<()> {
        let back = 1 - self.front;
        let retained = active_move.is_none()
            && self.targets[self.front].move_state.is_none()
            && self.targets[back].move_state.is_none();
        let damage = retained.then(|| merge_damage(self.targets[back].repair, scene.damage));
        self.render_target(
            &self.targets[back].buffer,
            &scene.nodes,
            buffers,
            active_move,
            damage,
        )?;
        self.targets[back].revision = scene.revision;
        self.targets[back].move_state = active_move.map(|(group, offset, _)| (group, offset));
        self.targets[back].repair = None;
        Ok(())
    }

    /// Queues the latest compositor-owned temporary window transform.
    pub fn compose_move(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: (u32, (i32, i32), u32),
    ) -> io::Result<()> {
        self.redraw_move_frame(nodes, buffers, active_move)
    }

    /// Queues the latest coalesced move transform after the previous flip completes.
    pub fn compose_latest_move(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: (u32, (i32, i32), u32),
    ) -> io::Result<()> {
        self.redraw_move_frame(nodes, buffers, active_move)
    }

    /// Moves the VirtIO-GPU hardware cursor without rendering or page flipping the scene.
    pub fn move_cursor(&mut self, position: (i32, i32)) -> io::Result<()> {
        if self.cursor_published {
            self.device.move_cursor(&self.topology, position)
        } else {
            self.cursor
                .present(&self.device, &self.topology, position)?;
            self.cursor_published = true;
            Ok(())
        }
    }

    /// Replaces the sole 2D hardware cursor resource with the selected shape.
    pub fn set_cursor_shape(&mut self, shape: u32, position: (i32, i32)) -> io::Result<()> {
        if !self.cursor.set_shape(shape) && self.cursor_published {
            return Ok(());
        }
        self.cursor
            .present(&self.device, &self.topology, position)?;
        self.cursor_published = true;
        Ok(())
    }

    fn redraw_move_frame(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: (u32, (i32, i32), u32),
    ) -> io::Result<()> {
        if !self.move_frames.request() {
            return Ok(());
        }
        let back = 1 - self.front;
        let revision = self.targets[self.front].revision;
        let (group, offset, _) = active_move;
        let damage = move_frame_damage(
            nodes,
            self.targets[back].revision,
            revision,
            self.targets[back].move_state,
            group,
            offset,
        );
        self.render_target(
            &self.targets[back].buffer,
            nodes,
            buffers,
            Some(active_move),
            damage,
        )?;
        self.targets[back].revision = revision;
        self.targets[back].move_state = Some((group, offset));
        self.targets[back].repair = None;
        self.device
            .page_flip(&self.topology, self.targets[back].framebuffer_id, revision)?;
        self.move_frames.submitted(revision);
        Ok(())
    }

    /// Reports whether one compositor-owned window-move flip is awaiting completion.
    ///
    /// # Returns
    ///
    /// `true` while the DRM event owner must remain in the compositor poll set.
    pub fn move_frame_in_flight(&self) -> bool {
        self.move_frames.is_in_flight()
    }

    /// Completes the sole window-move flip and returns its presentation event.
    ///
    /// # Returns
    ///
    /// The boolean is `true` when the move transform changed while this flip was
    /// in flight and the caller must queue one latest replacement. The event is
    /// the same guest-vblank completion used by ordinary scene presentation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when no move flip owns the DRM event, the event
    /// cannot be read, or its opaque sequence does not match the submitted frame.
    pub fn finish_move_frame(&mut self) -> io::Result<(bool, FlipEvent)> {
        if !self.move_frames.is_in_flight() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected move page-flip completion",
            ));
        }
        let event = self.device.read_flip_event()?;
        let latest_pending = self.move_frames.completed(event.user_data)?;
        self.front = 1 - self.front;
        Ok((latest_pending, event))
    }

    /// Flips the GPU-rendered back target for one accepted scene revision.
    pub fn present_scene(
        &mut self,
        revision: u64,
        damage: Rect,
        cursor: (i32, i32),
    ) -> io::Result<FlipEvent> {
        let old_front = self.front;
        let old_was_moving = self.targets[old_front].move_state.is_some();
        let event = self.present(revision)?;
        self.targets[old_front].repair = if old_was_moving {
            Some(Rect {
                x: 0,
                y: 0,
                width: self.targets[old_front].buffer.width(),
                height: self.targets[old_front].buffer.height(),
            })
        } else if damage.width != 0 && damage.height != 0 {
            Some(damage)
        } else {
            None
        };
        if !self.cursor_published {
            self.cursor.present(&self.device, &self.topology, cursor)?;
            self.cursor_published = true;
        }
        Ok(event)
    }

    fn present(&mut self, revision: u64) -> io::Result<FlipEvent> {
        if self.move_frames.is_in_flight() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "scene page flip raced a move page flip",
            ));
        }
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

    fn render_target(
        &self,
        target: &VirglResource,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
        damage: Option<Rect>,
    ) -> io::Result<()> {
        let mut layers = Vec::with_capacity(nodes.len() + usize::from(active_move.is_some()));
        if let Some((moving_group, offset, underlay_id)) = active_move {
            let underlay = buffers
                .get(underlay_id)
                .ok_or_else(scene_buffer_disappeared)?;
            let screen = Rect {
                x: 0,
                y: 0,
                width: underlay.width(),
                height: underlay.height(),
            };
            layers.push(texture_layer(underlay, screen, damage.unwrap_or(screen), &[], (0, 0)));
            let last_moving = nodes
                .iter()
                .rposition(|node| node.window_group == moving_group)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "move group disappeared"))?;
            for (index, node) in nodes.iter().enumerate() {
                let source = move_node_source(index, last_moving, node, moving_group);
                let Some(source) = source else {
                    continue;
                };
                let texture = match source {
                    MoveNodeSource::Original => buffers
                        .get(node.buffer_id)
                        .ok_or_else(scene_buffer_disappeared)?,
                    MoveNodeSource::Underlay => underlay,
                };
                let node_offset = if node.window_group == moving_group {
                    offset
                } else {
                    (0, 0)
                };
                layers.push(scene_layer(texture, node, node_offset, damage));
            }
        } else {
            for node in nodes {
                let texture = buffers
                    .get(node.buffer_id)
                    .ok_or_else(scene_buffer_disappeared)?;
                layers.push(scene_layer(texture, node, (0, 0), damage));
            }
        }
        if let Some(damage) = damage {
            self.renderer.clear_damage(target, damage)?;
        }
        self.renderer
            .render_layers(target, &layers, damage.is_none())
    }

    fn draw_boot(&mut self, target: usize) {
        let buffer = &mut self.targets[target].buffer;
        let mut canvas = unsafe {
            Canvas::new(
                buffer.as_mut_ptr().cast(),
                buffer.pitch(),
                buffer.width_usize(),
                buffer.height_usize(),
            )
        };
        canvas.draw_background();
        canvas.draw_bootlogo(&self.logo);
    }

    fn upload_boot(&self, target: usize) -> io::Result<()> {
        let buffer = &self.targets[target].buffer;
        buffer.transfer_to_host(0, 0, buffer.width(), buffer.height())
    }

    fn remove_targets(device: &DrmDevice, targets: &[Target; 2]) {
        for target in targets {
            let _ = device.remove_framebuffer(target.framebuffer_id);
        }
    }
}

impl Drop for Scanout {
    fn drop(&mut self) {
        Self::remove_targets(&self.device, &self.targets);
    }
}

fn take_master(device: &DrmDevice) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match device.set_master() {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(16) && attempts < 50 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveNodeSource {
    Original,
    Underlay,
}

fn move_node_source(
    index: usize,
    last_moving: usize,
    node: &Node,
    moving_group: u32,
) -> Option<MoveNodeSource> {
    if node.window_group == moving_group {
        return Some(MoveNodeSource::Original);
    }
    if index <= last_moving {
        return None;
    }
    Some(if node.kind == display_proto::SceneNodeKind::DisplayList {
        MoveNodeSource::Underlay
    } else {
        MoveNodeSource::Original
    })
}

fn scene_layer<'a>(
    texture: &'a VirglResource,
    node: &'a Node,
    offset: (i32, i32),
    damage: Option<Rect>,
) -> TextureLayer<'a> {
    let clip = translated(node.clip, offset);
    texture_layer(
        texture,
        translated(node.bounds, offset),
        damage.and_then(|damage| intersect(clip, damage)).unwrap_or_else(|| {
            if damage.is_some() {
                Rect::default()
            } else {
                clip
            }
        }),
        &node.clip_masks,
        offset,
    )
}

fn texture_layer<'a>(
    texture: &'a VirglResource,
    bounds: Rect,
    clip: Rect,
    clip_masks: &'a [display_proto::ClipMask],
    clip_offset: (i32, i32),
) -> TextureLayer<'a> {
    TextureLayer {
        texture,
        source: TextureRect {
            x: 0.0,
            y: 0.0,
            width: texture.width() as f32,
            height: texture.height() as f32,
        },
        bounds,
        clip,
        clip_masks,
        clip_offset,
        color: [1.0; 4],
        mode: crate::gpu::TextureMode::Color,
        sampling: crate::gpu::TextureSampling::Linear,
        wrap: crate::gpu::TextureWrap::Edge,
    }
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
}

fn move_damage(
    nodes: &[Node],
    window_group: u32,
    old_offset: (i32, i32),
    new_offset: (i32, i32),
) -> Option<Rect> {
    let group = nodes
        .iter()
        .filter(|node| node.window_group == window_group)
        .filter_map(|node| intersect(node.bounds, node.clip))
        .reduce(union)?;
    Some(union(
        translated(group, old_offset),
        translated(group, new_offset),
    ))
}

fn move_frame_damage(
    nodes: &[Node],
    target_revision: u64,
    scene_revision: u64,
    previous: Option<(u32, (i32, i32))>,
    window_group: u32,
    new_offset: (i32, i32),
) -> Option<Rect> {
    // A canonical target is not the zero-offset form of a move target: its
    // flattened desktop layer can contain this window's shadow/blur support
    // outside the scene-node clip. Treating `None` as `(0, 0)` leaves those
    // pixels outside partial damage and produces a permanent drag ghost.
    let (previous_group, old_offset) = previous?;
    (target_revision == scene_revision && previous_group == window_group)
        .then(|| move_damage(nodes, window_group, old_offset, new_offset))
        .flatten()
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

fn merge_damage(repair: Option<Rect>, changed: Rect) -> Rect {
    match repair {
        Some(repair) if repair.width == 0 || repair.height == 0 => changed,
        Some(repair)
            if repair.width != 0
                && repair.height != 0
                && changed.width != 0
                && changed.height != 0 =>
        {
            union(repair, changed)
        }
        Some(repair) => repair,
        None => changed,
    }
}

fn topology_size(topology: &Topology) -> Size {
    Size {
        width: u32::from(topology.mode.width()),
        height: u32::from(topology.mode.height()),
    }
}

fn scene_buffer_disappeared() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "scene buffer disappeared")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_transform_moves_bounds_and_clip_together() {
        let rect = Rect {
            x: 20,
            y: 30,
            width: 40,
            height: 50,
        };
        assert_eq!(translated(rect, (-5, 7)).x, 15);
        assert_eq!(translated(rect, (-5, 7)).y, 37);
    }

    #[test]
    fn move_damage_covers_the_target_owned_old_and_latest_offsets() {
        let nodes = [Node {
            kind: display_proto::SceneNodeKind::DisplayList,
            window_group: 7,
            buffer_id: 1,
            bounds: Rect {
                x: 20,
                y: 30,
                width: 80,
                height: 60,
            },
            clip: Rect {
                x: 30,
                y: 40,
                width: 50,
                height: 30,
            },
            clip_masks: Vec::new(),
        }];

        assert_eq!(
            move_damage(&nodes, 7, (10, 0), (40, 20)),
            Some(Rect {
                x: 40,
                y: 40,
                width: 80,
                height: 50,
            })
        );
    }

    #[test]
    fn canonical_target_is_fully_rebuilt_before_its_first_move_frame() {
        let nodes = [Node {
            kind: display_proto::SceneNodeKind::DisplayList,
            window_group: 7,
            buffer_id: 1,
            bounds: Rect {
                x: 0,
                y: 0,
                width: 300,
                height: 200,
            },
            clip: Rect {
                x: 40,
                y: 30,
                width: 120,
                height: 80,
            },
            clip_masks: Vec::new(),
        }];

        assert_eq!(
            move_frame_damage(&nodes, 11, 11, None, 7, (20, 10)),
            None,
            "canonical pixels outside the window clip require one full rebuild"
        );
        assert_eq!(
            move_frame_damage(&nodes, 11, 11, Some((7, (20, 10))), 7, (30, 25)),
            Some(Rect {
                x: 60,
                y: 40,
                width: 130,
                height: 95,
            })
        );
    }

    #[test]
    fn alternating_scanout_repairs_old_and_current_scene_damage() {
        let old = Rect {
            x: 100,
            y: 200,
            width: 80,
            height: 60,
        };
        let current = Rect {
            x: 160,
            y: 180,
            width: 100,
            height: 50,
        };
        assert_eq!(
            merge_damage(Some(old), current),
            Rect {
                x: 100,
                y: 180,
                width: 160,
                height: 80,
            }
        );
        assert_eq!(merge_damage(None, current), current);
        assert_eq!(merge_damage(Some(old), Rect::default()), old);
    }

    #[test]
    fn move_replays_only_the_moving_group_and_nodes_above_it() {
        let node = |kind, window_group| Node {
            kind,
            window_group,
            buffer_id: window_group + 1,
            bounds: Rect::default(),
            clip: Rect::default(),
            clip_masks: Vec::new(),
        };
        let nodes = [
            node(display_proto::SceneNodeKind::DisplayList, 0),
            node(display_proto::SceneNodeKind::DisplayList, 7),
            node(display_proto::SceneNodeKind::ForeignSurface, 7),
            node(display_proto::SceneNodeKind::DisplayList, 9),
            node(display_proto::SceneNodeKind::ForeignSurface, 9),
        ];
        let last_moving = 2;
        assert_eq!(move_node_source(0, last_moving, &nodes[0], 7), None);
        assert_eq!(
            move_node_source(1, last_moving, &nodes[1], 7),
            Some(MoveNodeSource::Original)
        );
        assert_eq!(
            move_node_source(2, last_moving, &nodes[2], 7),
            Some(MoveNodeSource::Original)
        );
        assert_eq!(
            move_node_source(3, last_moving, &nodes[3], 7),
            Some(MoveNodeSource::Underlay)
        );
        assert_eq!(
            move_node_source(4, last_moving, &nodes[4], 7),
            Some(MoveNodeSource::Original)
        );
    }

    #[test]
    fn move_burst_keeps_only_one_in_flight_and_one_latest_frame() {
        let mut frames = MoveFrames::default();
        assert!(frames.request());
        frames.submitted(7);

        for _ in 0..2_000 {
            assert!(!frames.request());
        }

        assert!(frames.completed(7).unwrap());
        assert!(frames.request());
        frames.submitted(7);
        assert!(!frames.completed(7).unwrap());
    }

    #[test]
    fn move_completion_rejects_an_unowned_flip() {
        let mut frames = MoveFrames::default();
        assert!(frames.request());
        frames.submitted(11);

        assert_eq!(
            frames.completed(12).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
