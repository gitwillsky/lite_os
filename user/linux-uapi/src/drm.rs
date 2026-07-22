//! Typed DRM dumb-buffer and modesetting resources.

mod shared;

pub use shared::SharedDumbBuffer;

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
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

/// One completed DRM page flip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlipEvent {
    /// Opaque sequence supplied to [`DrmDevice::page_flip`].
    pub user_data: u64,
    /// Kernel monotonic seconds at completion.
    pub seconds: u32,
    /// Remaining monotonic microseconds at completion.
    pub microseconds: u32,
    /// Device presentation sequence.
    pub sequence: u32,
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

    /// Maps a compositor-owned dumb buffer through a shared DRM file description.
    ///
    /// # Parameters
    ///
    /// - `raw_handle`: GEM handle published by the compositor.
    /// - `width`: Mapped pixel width.
    /// - `height`: Mapped pixel height.
    /// - `pitch`: Bytes between adjacent rows.
    /// - `byte_len`: Exact mapping length published by the compositor.
    ///
    /// # Returns
    ///
    /// A mapping that never destroys the compositor-owned GEM handle.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for inconsistent geometry or the Linux mapping error.
    pub fn map_shared_dumb(
        &self,
        raw_handle: u32,
        width: usize,
        height: usize,
        pitch: usize,
        byte_len: usize,
    ) -> io::Result<SharedDumbBuffer> {
        let handle = GemHandle::new(raw_handle)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "GEM handle is zero"))?;
        if pitch < width.saturating_mul(4) || pitch.saturating_mul(height) > byte_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared dumb-buffer geometry is inconsistent",
            ));
        }
        Ok(SharedDumbBuffer::new(
            self.map(handle, byte_len)?,
            pitch,
            width,
            height,
        ))
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

    /// Queues one event-producing page flip on the selected CRTC.
    ///
    /// # Parameters
    ///
    /// - `topology`: Topology whose CRTC receives the new framebuffer.
    /// - `framebuffer_id`: Previously registered framebuffer.
    /// - `user_data`: Opaque sequence returned in the completion event.
    ///
    /// # Returns
    ///
    /// `()` once the kernel accepted the asynchronous flip.
    ///
    /// # Errors
    ///
    /// Returns the Linux ioctl error for invalid ownership, framebuffer or in-flight state.
    pub fn page_flip(
        &self,
        topology: &Topology,
        framebuffer_id: u32,
        user_data: u64,
    ) -> io::Result<()> {
        let mut flip = raw::DrmPageFlip {
            crtc_id: topology.crtc_id,
            framebuffer_id,
            flags: 1,
            user_data,
            ..raw::DrmPageFlip::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_PAGE_FLIP, (&raw mut flip).cast())
    }

    /// Reads exactly one page-flip completion event.
    ///
    /// # Returns
    ///
    /// The decoded event associated with this shared DRM file description.
    ///
    /// # Errors
    ///
    /// Returns an I/O error or `InvalidData` for a non-flip DRM event.
    pub fn read_flip_event(&self) -> io::Result<FlipEvent> {
        let mut bytes = [0u8; 32];
        (&*self.file).read_exact(&mut bytes)?;
        if read_u32(&bytes, 0) != 2 || read_u32(&bytes, 4) != bytes.len() as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DRM returned a non-flip event",
            ));
        }
        Ok(FlipEvent {
            user_data: read_u64(&bytes, 8),
            seconds: read_u32(&bytes, 16),
            microseconds: read_u32(&bytes, 20),
            sequence: read_u32(&bytes, 24),
        })
    }

    /// Removes one registered framebuffer id.
    ///
    /// # Parameters
    ///
    /// - `framebuffer_id`: Framebuffer identity returned by [`DrmDevice::add_framebuffer`].
    ///
    /// # Returns
    ///
    /// `()` after the id is no longer published.
    ///
    /// # Errors
    ///
    /// Returns the Linux ioctl error when the id is invalid or still active.
    pub fn remove_framebuffer(&self, framebuffer_id: u32) -> io::Result<()> {
        let mut framebuffer_id = framebuffer_id;
        self.ioctl(raw::DRM_IOCTL_MODE_RMFB, (&raw mut framebuffer_id).cast())
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

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed DRM event"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed DRM event"),
    )
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

pub(super) struct Mapping {
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
