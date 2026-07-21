//! DRM master, modesetting, scanout buffer, and damage submission.

use std::{io, thread, time::Duration};

use linux_uapi::drm::{Clip, DrmDevice, DumbBuffer};

const MAX_CLIPS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub const fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub fn is_empty(self) -> bool {
        self.x2 <= self.x1 || self.y2 <= self.y1
    }

    pub fn intersect(self, other: Rect) -> Rect {
        Rect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        }
    }

    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        (self.x1..self.x2).contains(&x) && (self.y1..self.y2).contains(&y)
    }

    pub fn width(self) -> i32 {
        self.x2 - self.x1
    }

    pub fn height(self) -> i32 {
        self.y2 - self.y1
    }
}

pub struct Frame {
    pixels: *mut u32,
    pitch: usize,
    width: usize,
    height: usize,
}

impl Frame {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn row(&mut self, y: usize) -> &mut [u32] {
        assert!(y < self.height);
        unsafe {
            std::slice::from_raw_parts_mut(
                (self.pixels as *mut u8).add(y * self.pitch).cast::<u32>(),
                self.width,
            )
        }
    }
}

#[derive(Clone, Copy)]
pub struct Mode {
    pub width: usize,
    pub height: usize,
}

pub struct Scanout {
    device: DrmDevice,
    framebuffer_id: u32,
    buffer: DumbBuffer,
    mode: Mode,
}

impl Scanout {
    pub fn open() -> Result<Self, ()> {
        Self::try_open().map_err(|_| ())
    }

    fn try_open() -> io::Result<Self> {
        let device = DrmDevice::open("/dev/dri/card0")?;
        let topology = device.query_topology()?;
        let mut retries = 0;
        loop {
            match device.set_master() {
                Ok(()) => break,
                Err(error) if error.raw_os_error() == Some(16) && retries < 50 => {
                    retries += 1;
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error),
            }
        }
        let buffer =
            device.create_dumb(topology.mode.width().into(), topology.mode.height().into())?;
        let framebuffer_id = device.add_framebuffer(&buffer, 24)?;
        device.set_crtc(&topology, framebuffer_id)?;
        Ok(Self {
            device,
            framebuffer_id,
            mode: Mode {
                width: buffer.width(),
                height: buffer.height(),
            },
            buffer,
        })
    }

    pub fn drm_device(&self) -> &DrmDevice {
        &self.device
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn frame(&mut self) -> Frame {
        Frame {
            pixels: self.buffer.as_mut_ptr(),
            pitch: self.buffer.pitch(),
            width: self.mode.width,
            height: self.mode.height,
        }
    }

    pub fn present(&self, damage: &[Rect]) {
        let screen = Rect::new(0, 0, self.mode.width as i32, self.mode.height as i32);
        let mut clips = [Clip {
            x1: 0,
            y1: 0,
            x2: 0,
            y2: 0,
        }; MAX_CLIPS];
        let mut count = 0;
        let mut union = None;
        for rect in damage {
            let clipped = rect.intersect(screen);
            if clipped.is_empty() {
                continue;
            }
            union = Some(union.map_or(clipped, |previous: Rect| previous.union(clipped)));
            if count < MAX_CLIPS {
                clips[count] = to_clip(clipped);
                count += 1;
            }
        }
        let Some(union) = union else {
            return;
        };
        if count == MAX_CLIPS && damage.len() > MAX_CLIPS {
            clips[0] = to_clip(union);
            count = 1;
        }
        let _ = self.device.dirty(self.framebuffer_id, &clips[..count]);
    }
}

fn to_clip(rect: Rect) -> Clip {
    Clip {
        x1: rect.x1 as u16,
        y1: rect.y1 as u16,
        x2: rect.x2 as u16,
        y2: rect.y2 as u16,
    }
}
