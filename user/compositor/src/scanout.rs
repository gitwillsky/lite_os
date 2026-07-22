//! Double scanout and boot/scene composition.

use std::{fs, io, thread, time::Duration};

use display_proto::{Rect, Size};
use linux_uapi::drm::{DrmDevice, DumbBuffer, FlipEvent, Topology};

use crate::{
    boot::Canvas,
    cursor::Cursor,
    session::{Buffers, Scene},
};

struct Target {
    framebuffer_id: u32,
    buffer: DumbBuffer,
}

/// Unique DRM owner with two scanout buffers.
pub struct Scanout {
    device: DrmDevice,
    topology: Topology,
    targets: [Target; 2],
    front: usize,
    logo: Vec<u8>,
    cursor: Cursor,
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

    /// Composes the accepted flat scene into the back scanout.
    pub fn compose(
        &mut self,
        scene: &Scene,
        buffers: &Buffers,
        cursor: (i32, i32),
    ) -> io::Result<()> {
        let target = &mut self.targets[1 - self.front].buffer;
        for row in 0..target.height() {
            target.row_mut(row).fill(0);
        }
        let screen = Rect {
            x: 0,
            y: 0,
            width: target.width() as u32,
            height: target.height() as u32,
        };
        for node in &scene.nodes {
            let Some(source) = buffers.get(node.buffer_id) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "scene buffer disappeared",
                ));
            };
            composite_node(target, source, node.bounds, node.clip, screen);
        }
        self.cursor.paint(target, cursor.0, cursor.1);
        Ok(())
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
    bounds: Rect,
    clip: Rect,
    screen: Rect,
) {
    let x1 = bounds.x.max(clip.x).max(screen.x).max(0);
    let y1 = bounds.y.max(clip.y).max(screen.y).max(0);
    let x2 = (bounds.x + bounds.width as i32)
        .min(clip.x + clip.width as i32)
        .min(screen.width as i32);
    let y2 = (bounds.y + bounds.height as i32)
        .min(clip.y + clip.height as i32)
        .min(screen.height as i32);
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    for y in y1..y2 {
        let source_y = (y - bounds.y) as usize;
        let source_row = source.row(source_y);
        let target_row = target.row_mut(y as usize);
        for x in x1..x2 {
            let source_pixel = source_row[(x - bounds.x) as usize];
            target_row[x as usize] = over(source_pixel, target_row[x as usize]);
        }
    }
}

fn over(source: u32, destination: u32) -> u32 {
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
