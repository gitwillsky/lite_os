mod command;

pub use command::{CommandEncoder, ObjectKind, SamplerFilter, SamplerWrap, ShaderStage};

use std::{ffi::CString, io, mem::size_of};

use crate::raw;

use super::{DrmDevice, GemHandle, Mapping, SharedDumbBuffer};

const CAPSET_VIRGL2: u32 = 2;
const CAPSET_VIRGL2_VERSION: u32 = 2;
const CAPSET_BYTES: usize = 4096;
const CONTEXT_PARAM_CAPSET_ID: u64 = 1;
const CONTEXT_PARAM_DEBUG_NAME: u64 = 4;
const WAIT_NOWAIT: u32 = 1;

/// Gallium `PIPE_TEXTURE_2D` resource target。
pub const PIPE_TEXTURE_2D: u32 = 2;
/// Gallium `PIPE_BUFFER` resource target。
pub const PIPE_BUFFER: u32 = 0;
/// VirGL `B8G8R8A8_UNORM` format。
pub const FORMAT_B8G8R8A8_UNORM: u32 = 1;
/// VirGL `B8G8R8X8_UNORM` format。
pub const FORMAT_B8G8R8X8_UNORM: u32 = 2;
/// VirGL byte-addressable buffer format。
pub const FORMAT_R8_UNORM: u32 = 64;
/// Resource 可作为 render target。
pub const BIND_RENDER_TARGET: u32 = 1 << 1;
/// Resource 可作为 texture sampler source。
pub const BIND_SAMPLER_VIEW: u32 = 1 << 3;
/// Resource 可作为 vertex buffer。
pub const BIND_VERTEX_BUFFER: u32 = 1 << 4;
/// Resource 可直接交给 host display target。
pub const BIND_DISPLAY_TARGET: u32 = 1 << 8;
/// Resource 可交给 VirtIO-GPU scanout。
pub const BIND_SCANOUT: u32 = 1 << 14;
/// Resource 以浏览器/scanout 的 top-left origin 解释 Y 坐标。
pub const RESOURCE_Y_0_TOP: u32 = 1;

/// Immutable host capability bytes for the selected VirGL2 context ABI.
#[derive(Clone)]
pub struct VirglCapabilities {
    bytes: std::sync::Arc<[u8]>,
}

impl VirglCapabilities {
    /// Returns the exact fixed-size capability response buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// One initialized VirGL2 DRM context owned by the compositor OFD.
#[derive(Clone)]
pub struct VirglContext {
    device: DrmDevice,
    capabilities: VirglCapabilities,
}

impl VirglContext {
    /// Returns the host capabilities used to create this context.
    pub fn capabilities(&self) -> &VirglCapabilities {
        &self.capabilities
    }

    /// Creates one guest-backed 2D resource suitable for rendering and scanout.
    pub fn create_render_target(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::texture(
            width,
            height,
            FORMAT_B8G8R8X8_UNORM,
            BIND_RENDER_TARGET | BIND_SAMPLER_VIEW | BIND_DISPLAY_TARGET | BIND_SCANOUT,
        )?)
    }

    /// Creates one premultiplied BGRA texture sampled by the compositor.
    pub fn create_texture(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::texture(
            width,
            height,
            FORMAT_B8G8R8A8_UNORM,
            BIND_SAMPLER_VIEW | BIND_RENDER_TARGET,
        )?)
    }

    /// Creates one immutable R8 coverage texture for a compositor glyph atlas.
    pub fn create_mask_texture(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::texture(
            width,
            height,
            FORMAT_R8_UNORM,
            BIND_SAMPLER_VIEW,
        )?)
    }

    /// Creates one byte-addressed, guest-backed vertex buffer.
    pub fn create_vertex_buffer(&self, byte_len: u32) -> io::Result<VirglResource> {
        if byte_len == 0 {
            return Err(invalid_input("VirGL vertex buffer is empty"));
        }
        self.create_resource(ResourceSpec {
            target: PIPE_BUFFER,
            format: FORMAT_R8_UNORM,
            bind: BIND_VERTEX_BUFFER,
            width: byte_len,
            height: 1,
            size: byte_len,
            stride: 1,
            flags: 0,
        })
    }

    fn create_resource(&self, spec: ResourceSpec) -> io::Result<VirglResource> {
        let mut create = raw::VirtGpuResourceCreate {
            target: spec.target,
            format: spec.format,
            bind: spec.bind,
            width: spec.width,
            height: spec.height,
            depth: 1,
            array_size: 1,
            flags: spec.flags,
            size: u64::from(spec.size),
            ..raw::VirtGpuResourceCreate::default()
        };
        self.device.ioctl(
            raw::DRM_IOCTL_VIRTGPU_RESOURCE_CREATE,
            (&raw mut create).cast(),
        )?;
        let handle = GemHandle::new(create.bo_handle)
            .ok_or_else(|| invalid_data("VirtIO-GPU returned GEM handle zero"))?;
        if create.resource_handle == 0 {
            return Err(invalid_data("VirtIO-GPU returned resource handle zero"));
        }
        let mut info = raw::VirtGpuResourceInfo {
            bo_handle: handle.get(),
            ..raw::VirtGpuResourceInfo::default()
        };
        self.device
            .ioctl(raw::DRM_IOCTL_VIRTGPU_RESOURCE_INFO, (&raw mut info).cast())?;
        if info.resource_handle != create.resource_handle
            || info.size != spec.size
            || info.blob_memory != 0
        {
            return Err(invalid_data(
                "VirtIO-GPU returned inconsistent resource metadata",
            ));
        }
        let mapping = self.device.map_virgl(handle, spec.size as usize)?;
        Ok(VirglResource {
            device: self.device.clone(),
            handle,
            resource_id: create.resource_handle,
            mapping,
            format: spec.format,
            width: spec.width,
            height: spec.height,
            stride: spec.stride,
        })
    }

    /// Submits one 4-byte-aligned VirGL command stream asynchronously.
    pub fn exec(&self, commands: &[u32], resources: &[&VirglResource]) -> io::Result<()> {
        if commands.is_empty() {
            return Err(invalid_input("VirGL command stream is empty"));
        }
        if resources
            .iter()
            .any(|resource| !self.device.same_open_file(&resource.device))
        {
            return Err(invalid_input("VirGL resource belongs to another DRM OFD"));
        }
        let handles: Vec<u32> = resources
            .iter()
            .map(|resource| resource.handle.get())
            .collect();
        let mut exec = raw::VirtGpuExecBuffer {
            size: u32::try_from(std::mem::size_of_val(commands))
                .map_err(|_| invalid_input("VirGL command stream is too large"))?,
            command: commands.as_ptr() as u64,
            bo_handles: if handles.is_empty() {
                0
            } else {
                handles.as_ptr() as u64
            },
            num_bo_handles: handles.len() as u32,
            fence_fd: -1,
            ..raw::VirtGpuExecBuffer::default()
        };
        self.device
            .ioctl(raw::DRM_IOCTL_VIRTGPU_EXECBUFFER, (&raw mut exec).cast())
    }
}

struct ResourceSpec {
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    size: u32,
    stride: u32,
    flags: u32,
}

impl ResourceSpec {
    fn texture(width: u32, height: u32, format: u32, bind: u32) -> io::Result<Self> {
        let bytes_per_pixel = match format {
            FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM => 4,
            FORMAT_R8_UNORM => 1,
            _ => return Err(invalid_input("unsupported VirGL texture format")),
        };
        let stride = width
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_input("VirGL stride overflow"))?;
        let size = stride
            .checked_mul(height)
            .ok_or_else(|| invalid_input("VirGL resource size overflow"))?;
        if size == 0 {
            return Err(invalid_input("VirGL texture has zero area"));
        }
        Ok(Self {
            target: PIPE_TEXTURE_2D,
            format,
            bind,
            width,
            height,
            size,
            stride,
            flags: RESOURCE_Y_0_TOP,
        })
    }
}

/// One guest-backed VirGL resource and its stable GEM mapping.
pub struct VirglResource {
    device: DrmDevice,
    handle: GemHandle,
    resource_id: u32,
    mapping: Mapping,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

impl Drop for VirglResource {
    fn drop(&mut self) {
        let mut close = raw::DrmGemClose {
            handle: self.handle.get(),
            pad: 0,
        };
        let _ = self
            .device
            .ioctl(raw::DRM_IOCTL_GEM_CLOSE, (&raw mut close).cast());
    }
}

impl VirglResource {
    /// Returns the file-private GEM handle.
    pub fn handle(&self) -> GemHandle {
        self.handle
    }

    /// Returns the VirtIO-GPU resource identity used by VirGL command objects.
    pub fn resource_id(&self) -> u32 {
        self.resource_id
    }

    /// Returns the VirGL pipe format used to create this resource.
    pub fn format(&self) -> u32 {
        self.format
    }

    /// Returns the pixel width.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Returns the pixel height.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Returns bytes between adjacent scanlines.
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Returns the mapped backing size.
    pub fn size(&self) -> usize {
        self.mapping.size
    }

    /// Returns the pixel width as a host index.
    pub fn width_usize(&self) -> usize {
        self.width as usize
    }

    /// Returns the pixel height as a host index.
    pub fn height_usize(&self) -> usize {
        self.height as usize
    }

    /// Returns bytes between adjacent rows as a host index.
    pub fn pitch(&self) -> usize {
        self.stride as usize
    }

    /// Returns the immutable start of guest backing.
    pub fn as_ptr(&self) -> *const u32 {
        self.mapping.pointer.as_ptr().cast()
    }

    /// Returns the start of guest backing used for vertex, texture, or staging uploads.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.mapping.pointer.as_ptr()
    }

    /// Returns the whole mutable guest backing as bytes.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.mapping.pointer.as_ptr(), self.mapping.size) }
    }

    /// Returns one mutable XRGB8888 row.
    pub fn row_mut(&mut self, row: usize) -> &mut [u32] {
        assert!(row < self.height as usize);
        unsafe {
            std::slice::from_raw_parts_mut(
                self.mapping
                    .pointer
                    .as_ptr()
                    .add(row * self.stride as usize)
                    .cast(),
                self.width as usize,
            )
        }
    }

    /// Returns one immutable XRGB8888 row.
    pub fn row(&self, row: usize) -> &[u32] {
        assert!(row < self.height as usize);
        unsafe {
            std::slice::from_raw_parts(
                self.mapping
                    .pointer
                    .as_ptr()
                    .add(row * self.stride as usize)
                    .cast(),
                self.width as usize,
            )
        }
    }

    /// Uploads a guest-backed rectangle to the host resource asynchronously.
    pub fn transfer_to_host(&self, x: u32, y: u32, width: u32, height: u32) -> io::Result<()> {
        let offset = y
            .checked_mul(self.stride)
            .and_then(|offset| offset.checked_add(x.checked_mul(4)?))
            .ok_or_else(|| invalid_input("VirGL transfer offset overflow"))?;
        let mut transfer = raw::VirtGpuTransfer3d {
            bo_handle: self.handle.get(),
            region: raw::VirtGpuBox {
                x,
                y,
                z: 0,
                width,
                height,
                depth: 1,
            },
            offset,
            stride: self.stride,
            layer_stride: self
                .stride
                .checked_mul(self.height)
                .ok_or_else(|| invalid_input("VirGL layer stride overflow"))?,
            ..raw::VirtGpuTransfer3d::default()
        };
        self.device.ioctl(
            raw::DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST,
            (&raw mut transfer).cast(),
        )
    }

    /// Reads a host resource rectangle back into guest backing asynchronously.
    pub fn transfer_from_host(&self, x: u32, y: u32, width: u32, height: u32) -> io::Result<()> {
        let offset = y
            .checked_mul(self.stride)
            .and_then(|offset| offset.checked_add(x.checked_mul(4)?))
            .ok_or_else(|| invalid_input("VirGL transfer offset overflow"))?;
        let mut transfer = raw::VirtGpuTransfer3d {
            bo_handle: self.handle.get(),
            region: raw::VirtGpuBox {
                x,
                y,
                z: 0,
                width,
                height,
                depth: 1,
            },
            offset,
            stride: self.stride,
            layer_stride: self
                .stride
                .checked_mul(self.height)
                .ok_or_else(|| invalid_input("VirGL layer stride overflow"))?,
            ..raw::VirtGpuTransfer3d::default()
        };
        self.device.ioctl(
            raw::DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST,
            (&raw mut transfer).cast(),
        )
    }

    /// Uploads the complete byte range of a `PIPE_BUFFER` resource.
    pub fn transfer_buffer_to_host(&self) -> io::Result<()> {
        if self.height != 1 || self.stride != 1 || self.width as usize != self.mapping.size {
            return Err(invalid_input("VirGL resource is not a byte buffer"));
        }
        let mut transfer = raw::VirtGpuTransfer3d {
            bo_handle: self.handle.get(),
            region: raw::VirtGpuBox {
                x: 0,
                y: 0,
                z: 0,
                width: self.width,
                height: 1,
                depth: 1,
            },
            ..raw::VirtGpuTransfer3d::default()
        };
        self.device.ioctl(
            raw::DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST,
            (&raw mut transfer).cast(),
        )
    }

    /// Waits until the most recent exec or transfer referencing this resource completes.
    pub fn wait(&self) -> io::Result<()> {
        let mut wait = raw::VirtGpuWait {
            handle: self.handle.get(),
            ..raw::VirtGpuWait::default()
        };
        self.device
            .ioctl(raw::DRM_IOCTL_VIRTGPU_WAIT, (&raw mut wait).cast())
    }

    /// Returns whether the most recent resource fence has completed without blocking.
    pub fn is_ready(&self) -> io::Result<bool> {
        let mut wait = raw::VirtGpuWait {
            handle: self.handle.get(),
            flags: WAIT_NOWAIT,
        };
        match self
            .device
            .ioctl(raw::DRM_IOCTL_VIRTGPU_WAIT, (&raw mut wait).cast())
        {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::ResourceBusy => Ok(false),
            Err(error) => Err(error),
        }
    }
}

impl DrmDevice {
    /// Initializes the single compositor VirGL2 context on this DRM OFD.
    pub fn initialize_virgl(&self, debug_name: &str) -> io::Result<VirglContext> {
        let name =
            CString::new(debug_name).map_err(|_| invalid_input("VirGL debug name contains NUL"))?;
        if name.as_bytes().len() > 64 {
            return Err(invalid_input("VirGL debug name exceeds 64 bytes"));
        }
        if self.get_virgl_param(1)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "DRM device does not expose VirGL 3D",
            ));
        }
        let supported = self.get_virgl_param(7)?;
        if supported & (1 << CAPSET_VIRGL2) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "DRM device does not expose VirGL2",
            ));
        }
        let mut capabilities = vec![0; CAPSET_BYTES];
        let mut caps = raw::VirtGpuGetCaps {
            capset_id: CAPSET_VIRGL2,
            capset_version: CAPSET_VIRGL2_VERSION,
            address: capabilities.as_mut_ptr() as u64,
            size: capabilities.len() as u32,
            ..raw::VirtGpuGetCaps::default()
        };
        self.ioctl(raw::DRM_IOCTL_VIRTGPU_GET_CAPS, (&raw mut caps).cast())?;
        let parameters = [
            raw::VirtGpuContextParameter {
                parameter: CONTEXT_PARAM_CAPSET_ID,
                value: u64::from(CAPSET_VIRGL2),
            },
            raw::VirtGpuContextParameter {
                parameter: CONTEXT_PARAM_DEBUG_NAME,
                value: name.as_ptr() as u64,
            },
        ];
        let mut init = raw::VirtGpuContextInit {
            parameter_count: parameters.len() as u32,
            parameters: parameters.as_ptr() as u64,
            ..raw::VirtGpuContextInit::default()
        };
        if self.get_virgl_param(6)? != 0 {
            self.ioctl(raw::DRM_IOCTL_VIRTGPU_CONTEXT_INIT, (&raw mut init).cast())?;
        }
        Ok(VirglContext {
            device: self.clone(),
            capabilities: VirglCapabilities {
                bytes: capabilities.into(),
            },
        })
    }

    fn get_virgl_param(&self, parameter: u64) -> io::Result<u64> {
        let mut value = 0u64;
        let mut query = raw::VirtGpuGetParam {
            param: parameter,
            value: (&raw mut value) as u64,
        };
        self.ioctl(raw::DRM_IOCTL_VIRTGPU_GETPARAM, (&raw mut query).cast())?;
        Ok(value)
    }

    fn map_virgl(&self, handle: GemHandle, size: usize) -> io::Result<Mapping> {
        let mut map = raw::VirtGpuMap {
            handle: handle.get(),
            ..raw::VirtGpuMap::default()
        };
        self.ioctl(raw::DRM_IOCTL_VIRTGPU_MAP, (&raw mut map).cast())?;
        self.map_at_offset(map.offset, size)
    }

    /// Maps a compositor-owned VirGL GEM handle through the shared DRM OFD.
    pub fn map_shared_virgl(
        &self,
        raw_handle: u32,
        width: usize,
        height: usize,
        pitch: usize,
        byte_len: usize,
    ) -> io::Result<SharedDumbBuffer> {
        let handle =
            GemHandle::new(raw_handle).ok_or_else(|| invalid_input("VirGL GEM handle is zero"))?;
        if pitch < width.saturating_mul(4) || pitch.saturating_mul(height) > byte_len {
            return Err(invalid_input("shared VirGL geometry is inconsistent"));
        }
        Ok(SharedDumbBuffer::new(
            self.map_virgl(handle, byte_len)?,
            pitch,
            width,
            height,
        ))
    }

    /// Registers a VirGL render target as a KMS framebuffer.
    pub fn add_virgl_framebuffer(&self, resource: &VirglResource, depth: u32) -> io::Result<u32> {
        if !self.same_open_file(&resource.device) {
            return Err(invalid_input("VirGL resource belongs to another DRM OFD"));
        }
        let mut framebuffer = raw::DrmFramebuffer {
            width: resource.width,
            height: resource.height,
            pitch: resource.stride,
            bpp: 32,
            depth,
            handle: resource.handle.get(),
            ..raw::DrmFramebuffer::default()
        };
        self.ioctl(raw::DRM_IOCTL_MODE_ADDFB, (&raw mut framebuffer).cast())?;
        Ok(framebuffer.framebuffer_id)
    }

    fn same_open_file(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.file, &other.file)
    }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

const _: () = assert!(size_of::<u32>() == 4);
// VirGL freezes its own Gallium wire ABI; these values intentionally do not
// follow Mesa's current in-process `pipe_bind_flags` numbering.
const _: () = assert!(BIND_DISPLAY_TARGET == 1 << 8);
const _: () = assert!(BIND_SCANOUT == 1 << 14);
