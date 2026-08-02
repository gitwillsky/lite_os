//! GPU double scanout and atomic scene presentation.

use std::{fs, io, thread, time::Duration};

use display_proto::{DisplayListCommit, Rect, Size, TextureFormat, TextureRect};
use linux_uapi::drm::{DrmDevice, FlipEvent, Topology, VirglContext, VirglResource};

use crate::{
    boot::Canvas,
    cursor::Cursor,
    gpu::{GpuRenderer, TextureLayer},
    session::{Buffers, Node, Scene},
};

struct Target {
    framebuffer_id: u32,
    buffer: VirglResource,
    revision: u64,
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
        let cursor = Cursor::open(&graphics)?;
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
        };
        scanout.draw_boot(0);
        scanout.draw_boot(1);
        scanout.upload_boot(0)?;
        scanout.upload_boot(1)?;
        scanout
            .device
            .set_crtc(&scanout.topology, scanout.targets[0].framebuffer_id)?;
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

    /// Rasterizes one client display list into a compositor-owned GPU texture.
    pub fn render_display_list<'a>(
        &'a self,
        size: Size,
        list: DisplayListCommit<'_>,
        texture: impl FnMut(u32) -> Option<(&'a VirglResource, TextureFormat)>,
    ) -> io::Result<VirglResource> {
        let target = self.graphics.create_texture(size.width, size.height)?;
        self.renderer.render_display_list(&target, list, texture)?;
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
        self.render_target(&next[1].buffer, &scene.nodes, buffers, None, Some(cursor))?;
        next[0].revision = scene.revision;
        next[1].revision = scene.revision;
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
        eprintln!("compositor: GPU mode {}x{}", actual.width, actual.height);
        Ok(ModePresent::Presented(event))
    }

    /// Restores the pre-desktop boot image after an epoch reset.
    pub fn reset_to_boot(&mut self) -> io::Result<()> {
        self.draw_boot(0);
        self.draw_boot(1);
        self.upload_boot(0)?;
        self.upload_boot(1)?;
        for target in &mut self.targets {
            target.revision = 0;
        }
        self.cursor.set_shape(display_proto::CURSOR_DEFAULT);
        self.device
            .set_crtc(&self.topology, self.targets[self.front].framebuffer_id)
    }

    /// Uploads changed client textures and renders the accepted scene on the GPU.
    pub fn compose(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        let back = 1 - self.front;
        self.render_target(
            &self.targets[back].buffer,
            &scene.nodes,
            buffers,
            active_move,
            Some(cursor),
        )?;
        self.targets[back].revision = scene.revision;
        Ok(())
    }

    /// Re-renders a compositor-owned temporary window transform and flips it.
    pub fn compose_move(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: (u32, (i32, i32), u32),
        _damage: Rect,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        self.redraw_pointer_frame(nodes, buffers, Some(active_move), cursor)
    }

    /// Re-renders only GPU state for a pointer move; client textures are retained.
    pub fn move_cursor(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        self.redraw_pointer_frame(nodes, buffers, active_move, cursor)
    }

    /// Changes shape and immediately presents it through the same GPU pipeline.
    pub fn set_cursor_shape(
        &mut self,
        shape: u32,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        if !self.cursor.set_shape(shape) {
            return Ok(());
        }
        self.redraw_pointer_frame(nodes, buffers, active_move, cursor)
    }

    fn redraw_pointer_frame(
        &mut self,
        nodes: &[Node],
        buffers: &Buffers,
        active_move: Option<(u32, (i32, i32), u32)>,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        let back = 1 - self.front;
        self.render_target(
            &self.targets[back].buffer,
            nodes,
            buffers,
            active_move,
            Some(cursor),
        )?;
        let revision = self.targets[self.front].revision;
        self.targets[back].revision = revision;
        self.present(revision).map(|_| ())
    }

    /// Flips the GPU-rendered back target for one accepted scene revision.
    pub fn present_scene(&mut self, revision: u64, _cursor: (i32, i32)) -> io::Result<FlipEvent> {
        self.present(revision)
    }

    fn present(&mut self, revision: u64) -> io::Result<FlipEvent> {
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
        cursor: Option<(i32, i32)>,
    ) -> io::Result<()> {
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width(),
            height: target.height(),
        };
        let mut layers = Vec::with_capacity(nodes.len() + usize::from(cursor.is_some()));
        for node in nodes {
            let id = source_buffer_id(node, active_move);
            let texture = buffers.get(id).ok_or_else(scene_buffer_disappeared)?;
            let offset = active_move
                .filter(|(group, _, _)| *group == node.window_group)
                .map_or((0, 0), |(_, offset, _)| offset);
            layers.push(TextureLayer {
                texture,
                source: TextureRect {
                    x: 0.0,
                    y: 0.0,
                    width: texture.width() as f32,
                    height: texture.height() as f32,
                },
                bounds: translated(node.bounds, offset),
                clip: translated(node.clip, offset),
                clip_masks: &node.clip_masks,
                clip_offset: offset,
                color: [1.0; 4],
                mode: crate::gpu::TextureMode::Color,
                sampling: crate::gpu::TextureSampling::Linear,
                wrap: crate::gpu::TextureWrap::Edge,
            });
        }
        if let Some((x, y)) = cursor
            && let Some(layer) = self.cursor.layer(x, y, screen)
        {
            layers.push(layer);
        }
        self.renderer.render(target, &layers)
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

fn source_buffer_id(node: &Node, active_move: Option<(u32, (i32, i32), u32)>) -> u32 {
    active_move.map_or(node.buffer_id, |(window_group, _, underlay)| {
        if node.kind == display_proto::SceneNodeKind::DisplayList
            && node.window_group != window_group
        {
            underlay
        } else {
            node.buffer_id
        }
    })
}

fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
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
}
