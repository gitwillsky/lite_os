use core::fmt;
use alloc::{sync::Arc, string::String, vec::Vec, vec, boxed::Box};
use spin::Mutex;
use crate::drivers::{DeviceError, DeviceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    RGB888,   // 24-bit RGB
    RGBA8888, // 32-bit RGBA
    BGR888,   // 24-bit BGR
    BGRA8888, // 32-bit BGRA
    RGB565,   // 16-bit RGB
}

impl PixelFormat {
    pub fn bytes_per_pixel(&self) -> u32 {
        match self {
            PixelFormat::RGB888 | PixelFormat::BGR888 => 3,
            PixelFormat::RGBA8888 | PixelFormat::BGRA8888 => 4,
            PixelFormat::RGB565 => 2,
        }
    }

    pub fn bits_per_pixel(&self) -> u32 {
        self.bytes_per_pixel() * 8
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,           // 每行字节数
    pub format: PixelFormat,
    pub buffer_size: usize,
}

impl FramebufferInfo {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        let bytes_per_pixel = format.bytes_per_pixel();
        let pitch = width * bytes_per_pixel;
        let buffer_size = (pitch * height) as usize;

        FramebufferInfo {
            width,
            height,
            pitch,
            format,
            buffer_size,
        }
    }

    pub fn pixel_offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some((y * self.pitch + x * self.format.bytes_per_pixel()) as usize)
    }

    pub fn is_valid_coords(&self, x: u32, y: u32) -> bool {
        x < self.width && y < self.height
    }
}

pub trait Framebuffer: Send + Sync {
    fn info(&self) -> &FramebufferInfo;
    fn buffer_ptr(&self) -> *mut u8;
    fn buffer_size(&self) -> usize;

    fn write_pixel(&mut self, x: u32, y: u32, color: u32) -> Result<(), DeviceError>;
    fn read_pixel(&self, x: u32, y: u32) -> Result<u32, DeviceError>;
    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: u32) -> Result<(), DeviceError>;
    fn copy_rect(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32) -> Result<(), DeviceError>;
    fn clear(&mut self, color: u32) -> Result<(), DeviceError>;
    fn flush(&mut self) -> Result<(), DeviceError>;
    fn is_dirty(&self) -> bool;
    fn mark_dirty(&mut self);
    fn mark_clean(&mut self);
}

pub struct GenericFramebuffer {
    info: FramebufferInfo,
    buffer: usize,
    dirty: bool,
    flush_callback: Option<Box<dyn Fn() -> Result<(), DeviceError> + Send + Sync>>,
}

pub type VirtAddr = usize;
pub type PhysAddr = usize;

impl GenericFramebuffer {
    pub fn new(
        info: FramebufferInfo,
        buffer: usize,
        flush_callback: Option<Box<dyn Fn() -> Result<(), DeviceError> + Send + Sync>>
    ) -> Self {
        GenericFramebuffer {
            info,
            buffer,
            dirty: false,
            flush_callback,
        }
    }

    fn convert_color_to_format(&self, color: u32, format: PixelFormat) -> Vec<u8> {
        let r = ((color >> 16) & 0xFF) as u8;
        let g = ((color >> 8) & 0xFF) as u8;
        let b = (color & 0xFF) as u8;
        let a = ((color >> 24) & 0xFF) as u8;

        match format {
            PixelFormat::RGBA8888 => vec![r, g, b, a],
            PixelFormat::BGRA8888 => vec![b, g, r, a],
            PixelFormat::RGB888 => vec![r, g, b],
            PixelFormat::BGR888 => vec![b, g, r],
            PixelFormat::RGB565 => {
                let r5 = (r >> 3) & 0x1F;
                let g6 = (g >> 2) & 0x3F;
                let b5 = (b >> 3) & 0x1F;
                let rgb565 = ((r5 as u16) << 11) | ((g6 as u16) << 5) | (b5 as u16);
                vec![(rgb565 & 0xFF) as u8, (rgb565 >> 8) as u8]
            }
        }
    }

    fn convert_format_to_color(&self, bytes: &[u8], format: PixelFormat) -> u32 {
        match format {
            PixelFormat::RGBA8888 => {
                if bytes.len() >= 4 {
                    ((bytes[3] as u32) << 24) | ((bytes[0] as u32) << 16) |
                    ((bytes[1] as u32) << 8) | (bytes[2] as u32)
                } else {
                    0
                }
            }
            PixelFormat::BGRA8888 => {
                if bytes.len() >= 4 {
                    ((bytes[3] as u32) << 24) | ((bytes[2] as u32) << 16) |
                    ((bytes[1] as u32) << 8) | (bytes[0] as u32)
                } else {
                    0
                }
            }
            PixelFormat::RGB888 => {
                if bytes.len() >= 3 {
                    0xFF000000 | ((bytes[0] as u32) << 16) |
                    ((bytes[1] as u32) << 8) | (bytes[2] as u32)
                } else {
                    0
                }
            }
            PixelFormat::BGR888 => {
                if bytes.len() >= 3 {
                    0xFF000000 | ((bytes[2] as u32) << 16) |
                    ((bytes[1] as u32) << 8) | (bytes[0] as u32)
                } else {
                    0
                }
            }
            PixelFormat::RGB565 => {
                if bytes.len() >= 2 {
                    let rgb565 = (bytes[0] as u16) | ((bytes[1] as u16) << 8);
                    let r = ((rgb565 >> 11) & 0x1F) << 3;
                    let g = ((rgb565 >> 5) & 0x3F) << 2;
                    let b = (rgb565 & 0x1F) << 3;
                    0xFF000000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
                } else {
                    0
                }
            }
        }
    }
}

impl Framebuffer for GenericFramebuffer {
    fn info(&self) -> &FramebufferInfo {
        &self.info
    }

    fn buffer_ptr(&self) -> *mut u8 {
        self.buffer as *mut u8
    }

    fn buffer_size(&self) -> usize {
        self.info.buffer_size
    }

    fn write_pixel(&mut self, x: u32, y: u32, color: u32) -> Result<(), DeviceError> {
        if !self.info.is_valid_coords(x, y) {
            return Err(DeviceError::OperationFailed);
        }

        let offset = self.info.pixel_offset(x, y)
            .ok_or(DeviceError::OperationFailed)?;

        let color_bytes = self.convert_color_to_format(color, self.info.format);

        unsafe {
            let pixel_ptr = self.buffer_ptr().add(offset);
            for (i, &byte) in color_bytes.iter().enumerate() {
                *pixel_ptr.add(i) = byte;
            }
        }

        self.mark_dirty();
        Ok(())
    }

    fn read_pixel(&self, x: u32, y: u32) -> Result<u32, DeviceError> {
        if !self.info.is_valid_coords(x, y) {
            return Err(DeviceError::OperationFailed);
        }

        let offset = self.info.pixel_offset(x, y)
            .ok_or(DeviceError::OperationFailed)?;

        let bytes_per_pixel = self.info.format.bytes_per_pixel() as usize;
        let mut color_bytes = vec![0u8; bytes_per_pixel];

        unsafe {
            let pixel_ptr = self.buffer_ptr().add(offset);
            for i in 0..bytes_per_pixel {
                color_bytes[i] = *pixel_ptr.add(i);
            }
        }

        Ok(self.convert_format_to_color(&color_bytes, self.info.format))
    }

    fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: u32) -> Result<(), DeviceError> {
        let color_bytes = self.convert_color_to_format(color, self.info.format);
        let bytes_per_pixel = color_bytes.len();

        for dy in 0..height {
            let current_y = y + dy;
            if current_y >= self.info.height {
                break;
            }

            for dx in 0..width {
                let current_x = x + dx;
                if current_x >= self.info.width {
                    break;
                }

                let offset = self.info.pixel_offset(current_x, current_y)
                    .ok_or(DeviceError::OperationFailed)?;

                unsafe {
                    let pixel_ptr = self.buffer_ptr().add(offset);
                    for i in 0..bytes_per_pixel {
                        *pixel_ptr.add(i) = color_bytes[i];
                    }
                }
            }
        }

        self.mark_dirty();
        Ok(())
    }

    fn copy_rect(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, width: u32, height: u32) -> Result<(), DeviceError> {
        let bytes_per_pixel = self.info.format.bytes_per_pixel() as usize;

        if !self.info.is_valid_coords(src_x, src_y) ||
           !self.info.is_valid_coords(dst_x, dst_y) {
            return Err(DeviceError::OperationFailed);
        }

        for dy in 0..height {
            if src_y + dy >= self.info.height || dst_y + dy >= self.info.height {
                break;
            }

            for dx in 0..width {
                if src_x + dx >= self.info.width || dst_x + dx >= self.info.width {
                    break;
                }

                let src_offset = self.info.pixel_offset(src_x + dx, src_y + dy)
                    .ok_or(DeviceError::OperationFailed)?;
                let dst_offset = self.info.pixel_offset(dst_x + dx, dst_y + dy)
                    .ok_or(DeviceError::OperationFailed)?;

                unsafe {
                    let src_ptr = self.buffer_ptr().add(src_offset);
                    let dst_ptr = self.buffer_ptr().add(dst_offset);
                    core::ptr::copy_nonoverlapping(src_ptr, dst_ptr, bytes_per_pixel);
                }
            }
        }

        self.mark_dirty();
        Ok(())
    }

    fn clear(&mut self, color: u32) -> Result<(), DeviceError> {
        self.fill_rect(0, 0, self.info.width, self.info.height, color)
    }

    fn flush(&mut self) -> Result<(), DeviceError> {
        if self.dirty {
            if let Some(ref callback) = self.flush_callback {
                callback()?;
            }
            self.mark_clean();
        }
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn mark_clean(&mut self) {
        self.dirty = false;
    }
}

impl fmt::Debug for GenericFramebuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenericFramebuffer")
            .field("info", &self.info)
            .field("buffer", &format_args!("{:#x}", self.buffer))
            .field("dirty", &self.dirty)
            .finish()
    }
}

static GLOBAL_FRAMEBUFFER: Mutex<Option<Arc<Mutex<dyn Framebuffer>>>> = Mutex::new(None);

pub fn set_global_framebuffer(fb: Arc<Mutex<dyn Framebuffer>>) {
    *GLOBAL_FRAMEBUFFER.lock() = Some(fb);
}

pub fn get_global_framebuffer() -> Option<Arc<Mutex<dyn Framebuffer>>> {
    GLOBAL_FRAMEBUFFER.lock().clone()
}

pub fn with_global_framebuffer<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut dyn Framebuffer) -> R,
{
    if let Some(fb) = get_global_framebuffer() {
        let mut fb_guard = fb.lock();
        Some(f(&mut *fb_guard))
    } else {
        None
    }
}