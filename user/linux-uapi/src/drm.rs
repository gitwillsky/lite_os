//! Typed DRM dumb-buffer and modesetting resources.

use std::{
    fs::{File, OpenOptions},
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    os::unix::fs::OpenOptionsExt,
    ptr::NonNull,
    sync::Arc,
};

use crate::raw;

#[derive(Clone, Copy, Default)]
pub struct Mode(raw::DrmMode);

impl Mode {
    pub fn width(self) -> u16 {
        self.0.hdisplay
    }

    pub fn height(self) -> u16 {
        self.0.vdisplay
    }
}

pub struct Topology {
    pub crtc_id: u32,
    pub connector_id: u32,
    pub mode: Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GemHandle(u32);

impl GemHandle {
    pub fn new(raw: u32) -> Option<Self> {
        (raw != 0).then_some(Self(raw))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Clip {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

#[derive(Clone)]
pub struct DrmDevice {
    file: Arc<File>,
}

impl DrmDevice {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(raw::O_CLOEXEC)
            .open(path)?;
        Ok(Self {
            file: Arc::new(file),
        })
    }

    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self {
            file: Arc::new(File::from(fd)),
        }
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    pub fn query_topology(&self) -> io::Result<Topology> {
        let mut crtc_id = 0u32;
        let mut connector_id = 0u32;
        let mut resources = raw::DrmResources {
            crtc_ids: (&raw mut crtc_id) as u64,
            connector_ids: (&raw mut connector_id) as u64,
            crtc_count: 1,
            connector_count: 1,
            ..raw::DrmResources::default()
        };
        self.ioctl(
            raw::DRM_IOCTL_MODE_GETRESOURCES,
            (&raw mut resources).cast(),
        )?;
        if resources.crtc_count == 0 || resources.connector_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "DRM topology is empty",
            ));
        }
        let mut mode = raw::DrmMode::default();
        let mut connector = raw::DrmConnector {
            modes: (&raw mut mode) as u64,
            mode_count: 1,
            connector_id,
            ..raw::DrmConnector::default()
        };
        self.ioctl(
            raw::DRM_IOCTL_MODE_GETCONNECTOR,
            (&raw mut connector).cast(),
        )?;
        if connector.mode_count == 0 || mode.hdisplay == 0 || mode.vdisplay == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "DRM connector has no mode",
            ));
        }
        Ok(Topology {
            crtc_id,
            connector_id,
            mode: Mode(mode),
        })
    }

    pub fn set_master(&self) -> io::Result<()> {
        self.ioctl(raw::DRM_IOCTL_SET_MASTER, std::ptr::null_mut())
    }

    pub fn drop_master(&self) -> io::Result<()> {
        self.ioctl(raw::DRM_IOCTL_DROP_MASTER, std::ptr::null_mut())
    }

    pub fn create_dumb(&self, width: u32, height: u32) -> io::Result<DumbBuffer> {
        let mut create = raw::DrmDumbCreate {
            width,
            height,
            bpp: 32,
            ..raw::DrmDumbCreate::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_CREATE_DUMB, (&raw mut create).cast())?;
        let handle = GemHandle::new(create.handle).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "DRM returned handle zero")
        })?;
        let gem = OwnedGem {
            device: self.clone(),
            handle,
        };
        let size = usize::try_from(create.size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DRM size overflows usize"))?;
        let pitch = usize::try_from(create.pitch)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DRM pitch overflows usize"))?;
        let mapping = self.map(handle, size)?;
        let width_usize = width as usize;
        let height_usize = height as usize;
        if pitch < width_usize.saturating_mul(4) || pitch.saturating_mul(height_usize) > size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid DRM dumb geometry",
            ));
        }
        Ok(DumbBuffer {
            device: self.clone(),
            gem: Some(gem),
            mapping,
            pitch,
            width: width_usize,
            height: height_usize,
        })
    }

    pub fn adopt_transferred(
        &self,
        handle: GemHandle,
        width: usize,
        height: usize,
    ) -> io::Result<DumbBuffer> {
        let size = width
            .checked_mul(4)
            .and_then(|pitch| pitch.checked_mul(height))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "DRM size overflow"))?;
        let gem = OwnedGem {
            device: self.clone(),
            handle,
        };
        let mapping = self.map(handle, size)?;
        Ok(DumbBuffer {
            device: self.clone(),
            gem: Some(gem),
            mapping,
            pitch: width * 4,
            width,
            height,
        })
    }

    pub fn destroy_transferred(&self, handle: GemHandle) {
        drop(OwnedGem {
            device: self.clone(),
            handle,
        });
    }

    pub fn add_framebuffer(&self, buffer: &DumbBuffer, depth: u32) -> io::Result<u32> {
        let mut framebuffer = raw::DrmFramebuffer {
            width: buffer.width as u32,
            height: buffer.height as u32,
            pitch: buffer.pitch as u32,
            bpp: 32,
            depth,
            handle: buffer.handle().get(),
            ..raw::DrmFramebuffer::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_ADDFB, (&raw mut framebuffer).cast())?;
        Ok(framebuffer.framebuffer_id)
    }

    pub fn set_crtc(&self, topology: &Topology, framebuffer_id: u32) -> io::Result<()> {
        let connector_id = topology.connector_id;
        let mut crtc = raw::DrmCrtc {
            connectors: (&raw const connector_id) as u64,
            connector_count: 1,
            crtc_id: topology.crtc_id,
            framebuffer_id,
            mode_valid: 1,
            mode: topology.mode.0,
            ..raw::DrmCrtc::default()
        };
        loop {
            match self.ioctl(raw::DRM_IOCTL_MODE_SETCRTC, (&raw mut crtc).cast()) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    pub fn dirty(&self, framebuffer_id: u32, clips: &[Clip]) -> io::Result<()> {
        const _: () = assert!(size_of::<Clip>() == size_of::<raw::DrmClip>());
        const _: () = assert!(align_of::<Clip>() == align_of::<raw::DrmClip>());
        let mut dirty = raw::DrmDirty {
            framebuffer_id,
            flags: 0,
            color: 0,
            clip_count: clips.len() as u32,
            clips: clips.as_ptr() as u64,
        };
        loop {
            match self.ioctl(raw::DRM_IOCTL_MODE_DIRTYFB, (&raw mut dirty).cast()) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }

    fn map(&self, handle: GemHandle, size: usize) -> io::Result<Mapping> {
        let mut map = raw::DrmDumbMap {
            handle: handle.get(),
            ..raw::DrmDumbMap::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_MAP_DUMB, (&raw mut map).cast())?;
        let pointer = unsafe {
            raw::mmap(
                std::ptr::null_mut(),
                size,
                raw::PROT_READ | raw::PROT_WRITE,
                raw::MAP_SHARED,
                self.file.as_raw_fd(),
                map.offset as i64,
            )
        };
        if pointer as usize == usize::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping {
            pointer: NonNull::new(pointer.cast()).expect("mmap success is non-null"),
            size,
        })
    }

    fn ioctl(&self, request: usize, argument: *mut std::ffi::c_void) -> io::Result<()> {
        if unsafe { raw::ioctl(self.file.as_raw_fd(), request, argument) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

struct OwnedGem {
    device: DrmDevice,
    handle: GemHandle,
}

impl Drop for OwnedGem {
    fn drop(&mut self) {
        let mut handle = self.handle.get();
        let _ = self
            .device
            .ioctl(raw::DRM_IOCTL_MODE_DESTROY_DUMB, (&raw mut handle).cast());
    }
}

struct Mapping {
    pointer: NonNull<u8>,
    size: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = unsafe { raw::munmap(self.pointer.as_ptr().cast(), self.size) };
    }
}

pub struct DumbBuffer {
    device: DrmDevice,
    gem: Option<OwnedGem>,
    mapping: Mapping,
    pitch: usize,
    width: usize,
    height: usize,
}

impl DumbBuffer {
    pub fn handle(&self) -> GemHandle {
        self.gem
            .as_ref()
            .expect("GEM handle was transferred")
            .handle
    }

    pub fn transfer_handle(&mut self) -> GemHandle {
        self.gem
            .take()
            .expect("GEM handle transferred twice")
            .handle
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }

    pub fn size(&self) -> usize {
        self.mapping.size
    }

    pub fn as_ptr(&self) -> *const u32 {
        self.mapping.pointer.as_ptr().cast()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u32 {
        self.mapping.pointer.as_ptr().cast()
    }

    pub fn row_mut(&mut self, row: usize) -> &mut [u32] {
        assert!(row < self.height);
        unsafe {
            std::slice::from_raw_parts_mut(
                self.mapping
                    .pointer
                    .as_ptr()
                    .add(row * self.pitch)
                    .cast::<u32>(),
                self.width,
            )
        }
    }

    pub fn row(&self, row: usize) -> &[u32] {
        assert!(row < self.height);
        unsafe {
            std::slice::from_raw_parts(
                self.mapping
                    .pointer
                    .as_ptr()
                    .add(row * self.pitch)
                    .cast::<u32>(),
                self.width,
            )
        }
    }

    pub fn device(&self) -> &DrmDevice {
        &self.device
    }
}
