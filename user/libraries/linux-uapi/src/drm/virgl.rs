mod command;

pub use command::{CommandEncoder, ObjectKind, SamplerFilter, SamplerWrap, ShaderStage};

use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::CString,
    io,
    mem::size_of,
    os::fd::BorrowedFd,
    rc::{Rc, Weak},
};

use crate::raw;

use super::{DrmDevice, GemHandle, Mapping, SharedDumbBuffer};

const CAPSET_VIRGL2: u32 = 2;
const CAPSET_VIRGL2_VERSION: u32 = 2;
const CAPSET_BYTES: usize = 4096;
const CONTEXT_PARAM_CAPSET_ID: u64 = 1;
const CONTEXT_PARAM_DEBUG_NAME: u64 = 4;
const WAIT_NOWAIT: u32 = 1;
// Offsets in virglrenderer-1.1.0's virgl_caps_v2.
const CAPABILITY_BITS: usize = 392;
const CAPABILITY_BITS_V2: usize = 688;
const CAP_COPY_TRANSFER: u32 = 1 << 26;
const CAP_COPY_TRANSFER_BOTH_DIRECTIONS: u32 = 1 << 7;

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
/// 单次 `EXECBUFFER` 命令流的字节上限：Mesa virgl 命令缓冲最大值，与 LiteOS 内核 `VIRGL_COMMAND_MAX` 一致。
const EXEC_COMMAND_MAX: usize = (64 * 1024 + 1024) * 4;

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

    /// Reports whether a staged transfer can both upload and read back a texture.
    pub fn supports_staged_transfer(&self) -> bool {
        let bits = |offset| {
            u32::from_le_bytes(self.bytes[offset..][..4].try_into().expect("capset extent"))
        };
        bits(CAPABILITY_BITS) & CAP_COPY_TRANSFER != 0
            && bits(CAPABILITY_BITS_V2) & CAP_COPY_TRANSFER_BOTH_DIRECTIONS != 0
    }
}

/// One initialized VirGL2 DRM context owned by the compositor OFD.
#[derive(Clone)]
pub struct VirglContext {
    device: DrmDevice,
    capabilities: VirglCapabilities,
    // OWNER: 同 context 的 PRIME 导入共享一个 handle owner；Weak 不保活已释放客户端缓冲。
    imports: Rc<RefCell<BTreeMap<u32, Weak<GemOwner>>>>,
    // OWNER: 同一 context 的上传共用一块暂存，不在每张采样纹理旁保留完整 CPU 副本。
    // 只有最后传输 fence 完成才重写；忙时换新，旧 backing 由内核提交 pin 保活。
    // 缺少 fence 检查会覆盖设备尚未读取的像素；此缓存不引用 context，不形成所有权环。
    upload: Rc<RefCell<Option<VirglResource>>>,
}

impl VirglContext {
    fn own_handle(&self, handle: GemHandle) -> Rc<GemOwner> {
        let mut imports = self.imports.borrow_mut();
        if let Some(owner) = imports.get(&handle.get()).and_then(Weak::upgrade) {
            return owner;
        }
        imports.retain(|_, owner| owner.strong_count() != 0);
        let owner = Rc::new(GemOwner {
            device: self.device.clone(),
            handle,
        });
        imports.insert(handle.get(), Rc::downgrade(&owner));
        owner
    }

    /// 返回实际 DRM 设备的 Linux dev_t，供 linux-dmabuf 标准 feedback；metadata 失败返回 io::Error。
    pub fn device_number(&self) -> io::Result<u64> {
        use std::os::unix::fs::MetadataExt;
        Ok(self.device.file.metadata()?.rdev())
    }

    /// Returns the host capabilities used to create this context.
    pub fn capabilities(&self) -> &VirglCapabilities {
        &self.capabilities
    }

    /// Creates one guest-backed 2D resource suitable for rendering and scanout.
    ///
    /// The resource is `Y_0_TOP`: virglrenderer-1.1.0 stores it bottom-up in host GL
    /// (guest row 0 is the top GL row) and scans it out upright. Render into it with
    /// [`CommandEncoder::set_viewport`], whose GL-style Y axis maps NDC +1 to guest row 0.
    pub fn create_render_target(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::render_target(width, height)?)
    }

    /// Creates a full-resolution XRGB scanout texture with only the minimum GEM backing.
    ///
    /// CPU pixel access must go through a staging resource. Invalid dimensions or failed
    /// allocation and DRM ioctls return an error.
    pub fn create_scanout_target(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        // Linux VIRTGPU_RESOURCE_CREATE interprets size=0 as a single page. The renderer
        // allocates its texture from the independent width, height, and format fields.
        self.create_resource(ResourceSpec {
            size: 0,
            ..ResourceSpec::render_target(width, height)?
        })
    }

    /// Creates one premultiplied BGRA texture sampled by the compositor.
    ///
    /// Sampled-only resources carry no `Y_0_TOP`, so guest row 0 is texture coordinate
    /// `t = 0`. With that flag virglrenderer inverts uploads and the sampled image
    /// would be vertically flipped relative to its quad.
    pub fn create_texture(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::texture(
            width,
            height,
            FORMAT_B8G8R8A8_UNORM,
            BIND_SAMPLER_VIEW | BIND_RENDER_TARGET,
            0,
        )?)
    }

    /// Creates a sampled ARGB texture whose CPU uploads use context-owned staging.
    ///
    /// Dimensions and texture format match `create_texture`; only a minimum GEM page is
    /// retained. Direct CPU pixel access requires a full backing or a staged readback.
    /// Invalid dimensions or failed DRM allocation return an error.
    pub fn create_sampled_texture(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec {
            size: 0,
            ..ResourceSpec::texture(
                width,
                height,
                FORMAT_B8G8R8A8_UNORM,
                BIND_SAMPLER_VIEW | BIND_RENDER_TARGET,
                0,
            )?
        })
    }

    /// Creates one immutable R8 coverage texture for a compositor glyph atlas.
    pub fn create_mask_texture(&self, width: u32, height: u32) -> io::Result<VirglResource> {
        self.create_resource(ResourceSpec::texture(
            width,
            height,
            FORMAT_R8_UNORM,
            BIND_SAMPLER_VIEW,
            0,
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
            size: spec.size,
            stride: spec.stride,
            ..raw::VirtGpuResourceCreate::default()
        };
        self.device.ioctl(
            raw::DRM_IOCTL_VIRTGPU_RESOURCE_CREATE,
            (&raw mut create).cast(),
        )?;
        let handle = GemHandle::new(create.bo_handle)
            .ok_or_else(|| invalid_data("VirtIO-GPU returned GEM handle zero"))?;
        let owner = self.own_handle(handle);
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
            || info.size < spec.size
            || info.blob_memory != 0
        {
            return Err(invalid_data(
                "VirtIO-GPU returned inconsistent resource metadata",
            ));
        }
        let backing_size = if spec.size == 0 { info.size } else { spec.size };
        let mapping = self.device.map_virgl(handle, backing_size as usize)?;
        Ok(VirglResource {
            _owner: owner,
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

    /// Transfers a packed ARGB/XRGB rectangle between a host texture and another GEM's backing.
    ///
    /// `rect` is [x, y, width, height] in texture pixels. `readback` selects texture to
    /// backing. The staging resource must cover the complete packed rectangle from offset
    /// zero. Wait on it before the CPU reads or reuses its bytes.
    ///
    /// Returns InvalidInput for invalid format or bounds, Unsupported if the required
    /// VirGL capabilities are absent, or the submission error.
    pub fn staged_transfer(
        &self,
        texture: &VirglResource,
        staging: &VirglResource,
        rect: [u32; 4],
        readback: bool,
    ) -> io::Result<()> {
        let [x, y, width, height] = rect;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| invalid_input("staged transfer stride overflow"))?;
        if !matches!(
            texture.format,
            FORMAT_B8G8R8X8_UNORM | FORMAT_B8G8R8A8_UNORM
        ) || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|end| end > texture.width)
            || y.checked_add(height).is_none_or(|end| end > texture.height)
            || u64::from(stride) * u64::from(height) > staging.size() as u64
        {
            return Err(invalid_input("staged transfer rectangle is invalid"));
        }
        if !self.capabilities.supports_staged_transfer() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let mut commands = CommandEncoder::new();
        commands.copy_transfer(
            texture.resource_id,
            staging.resource_id,
            rect,
            stride,
            readback,
        );
        self.exec(commands.words(), &[texture, staging])
    }

    /// Uploads a packed rectangle using the context's reusable staging backing.
    ///
    /// `rect` is [x, y, width, height]. `fill` writes the packed texels from byte offset
    /// zero before publication. It may fail without modifying the destination texture.
    /// The callback's error is preserved; allocation and submission errors convert to E.
    pub fn stage_texture<E: From<io::Error>>(
        &self,
        texture: &VirglResource,
        rect: [u32; 4],
        fill: impl FnOnce(&VirglResource) -> Result<(), E>,
    ) -> Result<(), E> {
        let bytes = rect[2]
            .checked_mul(rect[3])
            .and_then(|count| count.checked_mul(4))
            .filter(|count| *count != 0)
            .ok_or_else(|| invalid_input("staged upload size is invalid"))?;
        let cached = self.upload.borrow_mut().take();
        let staging = match cached {
            Some(staging) if staging.size() >= bytes as usize && staging.is_ready()? => staging,
            _ => self.create_vertex_buffer(bytes)?,
        };
        // 在 fill 期间不持 RefCell 借用；失败时尚未提交，局部暂存随返回释放。
        fill(&staging)?;
        self.staged_transfer(texture, &staging, rect, false)?;
        *self.upload.borrow_mut() = Some(staging);
        Ok(())
    }

    /// 将标准 PRIME dma-buf 导入当前 context，作为直接采样纹理，不执行 CPU 拷贝或 transfer 上传。
    /// width/height/stride/format 是 linux-dmabuf 声明；当前支持紧密排列 B8G8R8A8/X8、offset 0 和隐式 modifier。
    /// 返回带共享 GEM owner 的视图；同一 fd 可以建立不同 ARGB/XRGB 视图。导入/几何/映射失败返回 io::Error。
    pub fn import_texture(
        &self,
        fd: BorrowedFd<'_>,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> io::Result<Rc<VirglResource>> {
        if width == 0
            || height == 0
            || width.checked_mul(4) != Some(stride)
            || !matches!(format, FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM)
        {
            return Err(invalid_input("unsupported dma-buf layout"));
        }
        let handle = self.device.import_dma_buf(fd)?;
        let owner = self.own_handle(handle);
        let prepare = || -> io::Result<VirglResource> {
            let mut info = raw::VirtGpuResourceInfo {
                bo_handle: handle.get(),
                ..Default::default()
            };
            self.device
                .ioctl(raw::DRM_IOCTL_VIRTGPU_RESOURCE_INFO, (&raw mut info).cast())?;
            if stride
                .checked_mul(height)
                .is_none_or(|size| size > info.size)
                || info.resource_handle == 0
                || info.blob_memory != 0
            {
                return Err(invalid_input("dma-buf does not cover the declared texture"));
            }
            Ok(VirglResource {
                _owner: owner,
                device: self.device.clone(),
                handle,
                resource_id: info.resource_handle,
                mapping: self.device.map_virgl(handle, info.size as usize)?,
                width,
                height,
                stride,
                format,
            })
        };
        prepare().map(Rc::new)
    }

    /// Writes a texel rectangle into a sampled texture without waiting on its fences.
    ///
    /// 1. An idle texture with complete CPU backing takes the pixels through guest backing and
    ///    `TRANSFER_TO_HOST`, which suits large regions.
    /// 2. A texture still referenced by a pending command may have a transfer reading its backing,
    ///    so the pixels go inline through the command stream (`RESOURCE_INLINE_WRITE`), split into
    ///    submissions within both the 16-bit packet length and kernel command-stream limit.
    ///    Textures with minimum backing use the context staging for regions larger than a
    ///    single inline submission. The host applies them in order: earlier draws keep the
    ///    old texels, later draws see the new ones. This mirrors Mesa virgl's busy-resource upload.
    ///
    /// # Parameters
    ///
    /// - `texture`: A 4-byte-per-texel texture created on this context.
    /// - `x`, `y`, `width`, `height`: Destination rectangle inside `texture`.
    /// - `pixels`: Exactly `width * height` texels, row-major without padding.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a mismatched or out-of-bounds rectangle, or the first ioctl error.
    pub fn write_texture(
        &self,
        texture: &VirglResource,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        pixels: &[u32],
    ) -> io::Result<()> {
        let fits = x.checked_add(width).is_some_and(|end| end <= texture.width)
            && y.checked_add(height)
                .is_some_and(|end| end <= texture.height);
        if !fits || pixels.len() != width as usize * height as usize {
            return Err(invalid_input("texture write rectangle is invalid"));
        }
        if width == 0 || height == 0 {
            return Ok(());
        }
        let row = width as usize;
        let full_backing = texture.size() >= texture.stride as usize * texture.height as usize;
        if full_backing && texture.is_ready()? {
            for (index, line) in pixels.chunks_exact(row).enumerate() {
                texture.write_row(y as usize + index, x as usize, line);
            }
            return texture.transfer_to_host(x, y, width, height);
        }
        let rows = CommandEncoder::inline_write_rows(row, EXEC_COMMAND_MAX / 4);
        if rows == 0 {
            return Err(invalid_input("texture row exceeds one command submission"));
        }
        // 一个内联包放不下的大区域使用共用暂存；小区域保留有序内联路径，避免增加暂存准入。
        if !full_backing && height as usize > rows {
            return self.stage_texture(texture, [x, y, width, height], |staging| {
                staging.write_words(0, pixels);
                Ok(())
            });
        }
        for (index, band) in pixels.chunks(rows * row).enumerate() {
            let mut commands = CommandEncoder::new();
            commands.inline_write(
                texture.resource_id,
                x,
                y + (index * rows) as u32,
                width,
                (band.len() / row) as u32,
                band,
            );
            self.exec(commands.words(), &[texture])?;
        }
        Ok(())
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
    fn render_target(width: u32, height: u32) -> io::Result<Self> {
        Self::texture(
            width,
            height,
            FORMAT_B8G8R8X8_UNORM,
            BIND_RENDER_TARGET | BIND_SAMPLER_VIEW | BIND_DISPLAY_TARGET | BIND_SCANOUT,
            RESOURCE_Y_0_TOP,
        )
    }

    fn texture(width: u32, height: u32, format: u32, bind: u32, flags: u32) -> io::Result<Self> {
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
            flags,
        })
    }
}

/// One guest-backed VirGL resource and its stable GEM mapping.
pub struct VirglResource {
    // OWNER: 同一 file-local GEM handle 只有一个关闭 owner；布局/格式各自属于 texture view。
    _owner: Rc<GemOwner>,
    device: DrmDevice,
    handle: GemHandle,
    resource_id: u32,
    mapping: Mapping,
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

struct GemOwner {
    device: DrmDevice,
    handle: GemHandle,
}
impl Drop for GemOwner {
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
    /// Copies aligned words into guest backing without borrowing the DMA-visible storage.
    ///
    /// `offset` counts 32-bit words. Callers must wait for earlier transfers before reuse.
    /// Panics if the word range exceeds the live mapping.
    pub fn write_words(&self, offset: usize, words: &[u32]) {
        assert!(
            offset
                .checked_add(words.len())
                .is_some_and(|end| end <= self.mapping.size / 4)
        );
        // SAFETY: the aligned mapping owns the complete checked range. No shared-reference
        // API lends out its storage, and this resource is neither Send nor Sync.
        unsafe {
            std::ptr::copy_nonoverlapping(
                words.as_ptr(),
                self.mapping.pointer.as_ptr().cast::<u32>().add(offset),
                words.len(),
            );
        }
    }

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
        let start = self.row_offset(row, 0, self.width as usize);
        // SAFETY: row_offset checks that the full row fits the aligned guest mapping.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.mapping.pointer.as_ptr().add(start).cast(),
                self.width as usize,
            )
        }
    }

    /// Copies one XRGB8888 row segment out of guest backing.
    ///
    /// # Parameters
    ///
    /// - `row`: Scanline index below [`Self::height`].
    /// - `x`: First pixel; `x + target.len()` must not exceed [`Self::width`].
    /// - `target`: Receives the pixels.
    ///
    /// # Panics
    ///
    /// Panics when the segment lies outside the resource.
    pub fn read_row(&self, row: usize, x: usize, target: &mut [u32]) {
        let start = self.row_offset(row, x, target.len());
        // SAFETY: `row_offset` bounds the segment inside the live mapping. `&self` APIs only
        // copy through raw pointers and never lend references into backing, and the type is
        // neither Send nor Sync, so no concurrent writer can alias this read.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mapping.pointer.as_ptr().add(start).cast::<u32>(),
                target.as_mut_ptr(),
                target.len(),
            );
        }
    }

    /// Copies one XRGB8888 row segment into guest backing for a later [`Self::transfer_to_host`].
    ///
    /// Shared texture owners (sampler views kept for in-flight frames) can update backing
    /// without exclusive access; callers order the write after prior host transfers with
    /// [`Self::wait`].
    ///
    /// # Parameters
    ///
    /// - `row`: Scanline index below [`Self::height`].
    /// - `x`: First pixel; `x + pixels.len()` must not exceed [`Self::width`].
    /// - `pixels`: Source pixels.
    ///
    /// # Panics
    ///
    /// Panics when the segment lies outside the resource.
    pub fn write_row(&self, row: usize, x: usize, pixels: &[u32]) {
        let start = self.row_offset(row, x, pixels.len());
        // SAFETY: bounds as in `read_row`. No `&self` API lends a reference into backing and
        // `&mut self` borrows exclude this call, so no Rust reference observes the mutation;
        // the type is neither Send nor Sync, so writes cannot race another thread.
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                self.mapping.pointer.as_ptr().add(start).cast::<u32>(),
                pixels.len(),
            );
        }
    }

    fn row_offset(&self, row: usize, x: usize, len: usize) -> usize {
        assert!(row < self.height as usize);
        assert!(
            x.checked_add(len)
                .is_some_and(|end| end <= self.width as usize)
        );
        let start = row * self.stride as usize + x * 4;
        assert!(
            start
                .checked_add(len * 4)
                .is_some_and(|end| end <= self.mapping.size)
        );
        start
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
            stride: 0,
            layer_stride: 0,
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
            stride: 0,
            layer_stride: 0,
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
    /// Checks the DRM 3D feature and VirGL2 capset required by the compositor without creating a context.
    ///
    /// # Returns
    ///
    /// Unit when both capabilities are available.
    ///
    /// # Errors
    ///
    /// Returns Unsupported for a 2D-only device or missing VirGL2, or the capability-query I/O error.
    pub fn validate_virgl2(&self) -> io::Result<()> {
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
        Ok(())
    }

    /// Initializes the single compositor VirGL2 context on this DRM OFD.
    pub fn initialize_virgl(&self, debug_name: &str) -> io::Result<VirglContext> {
        let name =
            CString::new(debug_name).map_err(|_| invalid_input("VirGL debug name contains NUL"))?;
        if name.as_bytes().len() > 64 {
            return Err(invalid_input("VirGL debug name exceeds 64 bytes"));
        }
        self.validate_virgl2()?;
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
            imports: Rc::new(RefCell::new(BTreeMap::new())),
            upload: Rc::new(RefCell::new(None)),
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
