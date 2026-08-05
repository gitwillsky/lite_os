use alloc::sync::Arc;
use spin::Mutex;

use crate::{
    drivers::{VirglBox, VirglCapsetInfo, VirglCommand, VirglTransferDirection},
    fallible_tree::{FallibleMap, VacantEntry},
    memory::{DeviceBacking, FrameAllocationClass, PAGE_SIZE},
};

use super::{
    DrmError, DrmFile, DrmRetry, DrmSubmission, DrmWait, DumbBuffer,
    publication_order::{ReservationError, UnpublishedId},
};

const PIPE_BUFFER: u32 = 0;
const PIPE_TEXTURE_2D: u32 = 2;
const FORMAT_B8G8R8A8_UNORM: u32 = 1;
const FORMAT_B8G8R8X8_UNORM: u32 = 2;
const FORMAT_R8_UNORM: u32 = 64;

/// @description `DRM_IOCTL_VIRTGPU_RESOURCE_CREATE` 的无 pointer 领域输入。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VirglResourceCreate {
    pub(crate) target: u32,
    pub(crate) format: u32,
    pub(crate) bind: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) depth: u32,
    pub(crate) array_size: u32,
    pub(crate) last_level: u32,
    pub(crate) samples: u32,
    pub(crate) flags: u32,
    pub(crate) size: u64,
}

/// @description 一个 file-private VirGL GEM/resource 的稳定 UAPI metadata。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VirglResourceInfo {
    pub(crate) handle: u32,
    pub(crate) resource_id: u32,
    pub(crate) size: u32,
}

/// @description 一次 file-private VirGL resource transfer 的已解码领域输入。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VirglTransfer {
    /// 本 OFD 拥有的 GEM handle。
    pub(crate) handle: u32,
    /// guest backing 与 host resource 间的数据方向。
    pub(crate) direction: VirglTransferDirection,
    /// guest backing 中的起始 byte offset。
    pub(crate) offset: u32,
    /// resource mip level。
    pub(crate) level: u32,
    /// 非空三维半开区域。
    pub(crate) region: VirglBox,
    /// 相邻 row 的 byte 距离。
    pub(crate) stride: u32,
    /// 相邻 layer 的 byte 距离。
    pub(crate) layer_stride: u32,
}

pub(super) struct VirglContext {
    pub(super) id: u32,
    cleanup: Option<VacantEntry<u32, VirglCleanup>>,
}

impl VirglContext {
    /// @description 把 OFD-owned resource map 移入创建 context 时预分配的 cleanup node。
    pub(super) fn into_cleanup(
        mut self,
        buffers: FallibleMap<u32, Arc<VirglBuffer>>,
    ) -> VacantEntry<u32, VirglCleanup> {
        let mut cleanup = self
            .cleanup
            .take()
            .expect("published VirGL context lost cleanup node");
        cleanup.value_mut().buffers = buffers;
        cleanup
    }
}

/// @description DRM OFD 关闭后由 device completion owner 串行推进的 VirGL 回收队列。
pub(super) struct VirglCleanup {
    context_id: u32,
    buffers: FallibleMap<u32, Arc<VirglBuffer>>,
    in_flight: Option<(u64, VirglCleanupAction)>,
}

/// @description 一次 OFD 回收中当前可提交的标准 VirtIO-GPU operation。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VirglCleanupAction {
    Resource { handle: u32, resource_id: u32 },
    Context { context_id: u32 },
}

impl VirglCleanupAction {
    /// @description 降低为不包含 kernel owner 的标准 adapter command。
    pub(super) const fn command(self) -> VirglCommand<'static> {
        match self {
            Self::Resource { resource_id, .. } => VirglCommand::ResourceUnref { resource_id },
            Self::Context { context_id } => VirglCommand::ContextDestroy { context_id },
        }
    }
}

impl VirglCleanup {
    fn new(context_id: u32) -> Self {
        Self {
            context_id,
            buffers: FallibleMap::new(),
            in_flight: None,
        }
    }

    /// @description 在 adapter 空闲时选择一个 fence 已完成的 resource，全部释放后销毁 context。
    pub(super) fn next_action(&self, completed: u64) -> Option<VirglCleanupAction> {
        if self.in_flight.is_some() {
            return None;
        }
        if let Some((&handle, buffer)) = self
            .buffers
            .iter()
            .find(|(_, buffer)| *buffer.last_fence.lock() <= completed)
        {
            return Some(VirglCleanupAction::Resource {
                handle,
                resource_id: buffer.resource_id,
            });
        }
        self.buffers
            .is_empty()
            .then_some(VirglCleanupAction::Context {
                context_id: self.context_id,
            })
    }

    /// @description 记录已由 adapter 接受的唯一回收 operation 与 exact fence。
    pub(super) fn record_submission(&mut self, action: VirglCleanupAction, fence: u64) {
        assert!(self.in_flight.replace((fence, action)).is_none());
    }

    /// @description 判断 completion fence 是否属于本 cleanup queue 的当前 operation。
    pub(super) fn owns_fence(&self, fence: u64) -> bool {
        self.in_flight
            .is_some_and(|(expected, _)| expected == fence)
    }

    /// @description 消费 exact completion；返回 true 表示 context 已销毁且队列可删除。
    pub(super) fn complete(&mut self, fence: u64) -> bool {
        let Some((expected, action)) = self.in_flight else {
            return false;
        };
        if expected != fence {
            return false;
        }
        self.in_flight = None;
        match action {
            VirglCleanupAction::Resource { handle, .. } => {
                self.buffers
                    .remove(&handle)
                    .expect("submitted VirGL cleanup resource disappeared");
                false
            }
            VirglCleanupAction::Context { .. } => {
                assert!(self.buffers.is_empty());
                true
            }
        }
    }
}

pub(super) struct VirglBuffer {
    pub(super) resource_id: u32,
    pub(super) size: usize,
    pub(super) target: u32,
    pub(super) stride: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) backing: Arc<DeviceBacking>,
    pub(super) last_fence: Mutex<u64>,
}

/// @description 尚未发布、但已预留 device context identity 的 transaction。
pub(crate) struct PreparedVirglContext<'file> {
    file: &'file DrmFile,
    reservation: Option<UnpublishedId<u32>>,
    id: u32,
    capset_id: u32,
    name: [u8; 64],
    name_length: usize,
    cleanup: Option<VacantEntry<u32, VirglCleanup>>,
}

impl PreparedVirglContext<'_> {
    /// @description 返回应提交给 adapter 的标准 CTX_CREATE operation。
    /// @return context identity、capset init 与 debug name 全部绑定的 command。
    pub(crate) fn command(&self) -> VirglCommand<'_> {
        VirglCommand::ContextCreate {
            context_id: self.id,
            context_init: self.capset_id,
            name: &self.name[..self.name_length],
        }
    }

    /// @description 在 CTX_CREATE fence 完成后无失败发布 file context。
    pub(crate) fn publish(mut self) {
        let reservation = self
            .reservation
            .take()
            .expect("published VirGL context lost reservation");
        self.file.state.lock().context = Some(VirglContext {
            id: self.id,
            cleanup: self.cleanup.take(),
        });
        self.file
            .device
            .state
            .lock()
            .context_ids
            .publish(reservation);
    }
}

impl Drop for PreparedVirglContext<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.file
                .device
                .state
                .lock()
                .context_ids
                .rollback(reservation);
        }
        drop(self.cleanup.take());
    }
}

/// @description 已完成 backing、handle/resource identity 与 map node 预留的 transaction。
pub(crate) struct PreparedVirglResource<'file> {
    file: &'file DrmFile,
    handle: Option<UnpublishedId<u32>>,
    resource: Option<UnpublishedId<u32>>,
    handle_id: u32,
    resource_id: u32,
    context_id: u32,
    create: VirglResourceCreate,
    buffer: Option<Arc<VirglBuffer>>,
    entry: Option<VacantEntry<u32, Arc<VirglBuffer>>>,
}

/// @description 已从 file handle namespace 摘除、等待完成 host 销毁的 VirGL resource。
///
/// transaction 失败时 `Drop` 使用原 AVL node 无分配恢复 handle；成功时
/// `publish` 消费 backing。缺少该事务会让并发 exec 在 unref 后继续引用 resource，
/// 或让任一 host failure 永久丢失仍存活的 GEM object。
pub(crate) struct PreparedVirglClose<'file> {
    file: &'file DrmFile,
    entry: Option<VacantEntry<u32, Arc<VirglBuffer>>>,
    resource_id: u32,
    last_fence: u64,
}

impl PreparedVirglClose<'_> {
    pub(crate) fn wait(&self) -> Option<DrmWait> {
        (self.last_fence != 0).then(|| DrmWait {
            device: self.file.device.clone(),
            fence: self.last_fence,
        })
    }

    pub(crate) fn unref_command(&self) -> VirglCommand<'static> {
        VirglCommand::ResourceUnref {
            resource_id: self.resource_id,
        }
    }

    pub(crate) fn publish(mut self) {
        drop(
            self.entry
                .take()
                .expect("VirGL close lost resource entry")
                .into_value(),
        );
    }
}

impl Drop for PreparedVirglClose<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.file.state.lock().graphics_buffers.commit_vacant(entry);
        }
    }
}

impl PreparedVirglResource<'_> {
    /// @description 返回 host resource create operation。
    pub(crate) fn create_command(&self) -> VirglCommand<'static> {
        VirglCommand::ResourceCreate3d {
            resource_id: self.resource_id,
            target: self.create.target,
            format: self.create.format,
            bind: self.create.bind,
            width: self.create.width,
            height: self.create.height,
            depth: self.create.depth,
            array_size: self.create.array_size,
            last_level: self.create.last_level,
            samples: self.create.samples,
            flags: self.create.flags,
        }
    }

    /// @description 返回 guest backing attach operation。
    pub(crate) fn attach_command(&self) -> VirglCommand<'static> {
        VirglCommand::ResourceAttachBacking {
            resource_id: self.resource_id,
            backing: self
                .buffer
                .as_ref()
                .expect("prepared VirGL buffer missing")
                .backing
                .clone(),
        }
    }

    /// @description 返回 context/resource ownership attach operation。
    pub(crate) fn context_attach_command(&self) -> VirglCommand<'static> {
        VirglCommand::ContextAttachResource {
            context_id: self.context_id,
            resource_id: self.resource_id,
        }
    }

    /// @description 返回 UAPI copyout 所需的 stable identities。
    pub(crate) fn info(&self) -> VirglResourceInfo {
        VirglResourceInfo {
            handle: self.handle_id,
            resource_id: self.resource_id,
            size: self.create.size as u32,
        }
    }

    /// @description 在三个 host operation 与 UAPI copyout 全部成功后发布 GEM handle。
    pub(crate) fn publish(mut self) {
        let handle = self
            .handle
            .take()
            .expect("VirGL handle reservation missing");
        let resource = self
            .resource
            .take()
            .expect("VirGL resource reservation missing");
        let entry = self.entry.take().expect("VirGL map node missing");
        self.buffer.take().expect("VirGL buffer missing");
        self.file.state.lock().graphics_buffers.commit_vacant(entry);
        self.file.state.lock().handle_ids.publish(handle);
        self.file
            .device
            .state
            .lock()
            .graphics_resource_ids
            .publish(resource);
    }

    /// @description 返回 host rollback 所需 context/resource identities。
    pub(crate) const fn identities(&self) -> (u32, u32) {
        (self.context_id, self.resource_id)
    }
}

impl Drop for PreparedVirglResource<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.file.state.lock().handle_ids.rollback(handle);
        }
        if let Some(resource) = self.resource.take() {
            self.file
                .device
                .state
                .lock()
                .graphics_resource_ids
                .rollback(resource);
        }
    }
}

fn reservation_error(error: ReservationError) -> DrmError {
    match error {
        ReservationError::OutOfMemory => DrmError::OutOfMemory,
        ReservationError::NoSpace => DrmError::NoSpace,
    }
}

fn virgl_resource_stride(create: VirglResourceCreate) -> Result<u32, DrmError> {
    if create.size > u64::from(u32::MAX)
        || create.depth != 1
        || create.array_size != 1
        || create.last_level != 0
        || create.samples != 0
    {
        return Err(DrmError::Invalid);
    }
    if create.target == PIPE_BUFFER {
        if create.format != FORMAT_R8_UNORM
            || create.height != 1
            || u64::from(create.width) > create.size
        {
            return Err(DrmError::Invalid);
        }
        return Ok(1);
    }
    if create.target != PIPE_TEXTURE_2D {
        return Err(DrmError::Invalid);
    }
    let bytes_per_pixel = match create.format {
        FORMAT_B8G8R8A8_UNORM | FORMAT_B8G8R8X8_UNORM => 4,
        FORMAT_R8_UNORM => 1,
        _ => return Err(DrmError::Invalid),
    };
    let stride = create
        .width
        .checked_mul(bytes_per_pixel)
        .ok_or(DrmError::Invalid)?;
    let minimum = u64::from(stride)
        .checked_mul(u64::from(create.height))
        .ok_or(DrmError::Invalid)?;
    if create.size < minimum {
        return Err(DrmError::Invalid);
    }
    Ok(stride)
}

impl DrmFile {
    /// @description 取得并保活一个满足 VirtIO-GPU cursorq 固定 geometry 的 dumb buffer。
    /// @param handle 当前 OFD 的标准 DRM dumb GEM handle。
    /// @return 64x64x4 guest-backed pixel owner。
    /// @errors handle 不存在或 resource contract 不匹配。
    pub(super) fn cursor_resource(&self, handle: u32) -> Result<Arc<DumbBuffer>, DrmError> {
        let state = self.state.lock();
        let buffer = state.buffers.get(&handle).ok_or(DrmError::NotFound)?;
        if buffer.pitch != 64 * 4 || buffer.size != 64 * 64 * 4 {
            return Err(DrmError::Invalid);
        }
        Ok(buffer.clone())
    }

    /// @description 把标准 dumb cursor pixels 上传到 adapter-owned 2D resource。
    /// @param handle 当前 OFD 的 64x64x4 dumb GEM handle。
    /// @return 完整 2D CREATE/ATTACH/TRANSFER fence，或 adapter readiness retry。
    /// @errors 非 master、CRTC inactive、handle/geometry 非法或 device failure。
    pub(crate) fn upload_cursor(&self, handle: u32) -> Result<DrmSubmission, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let resource = self.cursor_resource(handle)?;
        let mut completion = self.device.completion.lock();
        if completion.active.is_none() {
            return Err(DrmError::Invalid);
        }
        let generation = completion.adapter_generation;
        let fence = match self
            .device
            .display
            .submit_cursor_upload(resource.identity, resource.backing.clone())
        {
            Ok(fence) => fence,
            Err(crate::drivers::DisplayError::WouldBlock) => {
                return Ok(DrmSubmission::Retry(DrmRetry {
                    device: self.device.clone(),
                    generation,
                }));
            }
            Err(error) => return Err(super::device::display_error(error)),
        };
        completion
            .timeline
            .submit(fence)
            .expect("published cursor upload fence exceeded completion timeline");
        drop(completion);
        Ok(DrmSubmission::Wait(DrmWait {
            device: self.device.clone(),
            fence,
        }))
    }

    /// @description 返回 host 固定的 VirGL capset ABI。
    pub(crate) fn virgl_capset_info(&self) -> Result<VirglCapsetInfo, DrmError> {
        self.device
            .display
            .virgl_capset_info()
            .ok_or(DrmError::Device)
    }

    /// @description 复制 immutable VirGL capset bytes。
    pub(crate) fn copy_virgl_capset(&self, output: &mut [u8]) -> Result<usize, DrmError> {
        self.device
            .display
            .copy_virgl_capset(output)
            .map_err(super::device::display_error)
    }

    /// @description 返回此 adapter 是否支持显式 VirGL context 参数。
    pub(crate) fn supports_virgl_context_init(&self) -> bool {
        self.device.display.supports_virgl_context_init()
    }

    /// @description 预留本 OFD 的唯一 VirGL context。
    /// @param capset_id 必须等于 adapter 选中的 VirGL capset。
    /// @param name 最多 64-byte explicit debug name。
    /// @return 尚未发布、可先提交 CTX_CREATE 的 transaction。
    /// @errors 重复初始化、capset 不匹配、名称过长或 identity OOM/耗尽。
    pub(crate) fn prepare_virgl_context(
        &self,
        capset_id: u32,
        name: &[u8],
    ) -> Result<PreparedVirglContext<'_>, DrmError> {
        if !self.supports_virgl_context_init()
            || name.len() > 64
            || self.virgl_capset_info()?.id != capset_id
        {
            return Err(DrmError::Invalid);
        }
        self.prepare_context(capset_id, name)
    }

    /// @description 按 Linux virtio-gpu legacy 语义准备首个 3D resource 的惰性 context。
    /// @return context 已存在时为 None；否则返回 `context_init=0` 的 CTX_CREATE transaction。
    pub(crate) fn prepare_legacy_virgl_context(
        &self,
    ) -> Result<Option<PreparedVirglContext<'_>>, DrmError> {
        self.virgl_capset_info()?;
        let state = self.state.lock();
        if state.context.is_some() {
            return Ok(None);
        }
        if !state.graphics_buffers.is_empty() {
            return Err(DrmError::Invalid);
        }
        drop(state);
        self.prepare_context(0, &[]).map(Some)
    }

    fn prepare_context(
        &self,
        capset_id: u32,
        name: &[u8],
    ) -> Result<PreparedVirglContext<'_>, DrmError> {
        let reservation = self
            .device
            .state
            .lock()
            .context_ids
            .reserve()
            .map_err(reservation_error)?;
        let id = reservation.id();
        let cleanup = match FallibleMap::try_prepare(id, VirglCleanup::new(id)) {
            Ok(cleanup) => cleanup,
            Err(_) => {
                self.device.state.lock().context_ids.rollback(reservation);
                return Err(DrmError::OutOfMemory);
            }
        };
        let mut encoded_name = [0; 64];
        encoded_name[..name.len()].copy_from_slice(name);
        Ok(PreparedVirglContext {
            file: self,
            reservation: Some(reservation),
            id,
            capset_id,
            name: encoded_name,
            name_length: name.len(),
            cleanup: Some(cleanup),
        })
    }

    /// @description 预留一个标准 VirGL resource 与 file-private GEM handle。
    /// @param create 已从 Linux UAPI 解码的 resource geometry/format/storage contract。
    /// @return host create/attach 完成前不可查询的 prepared transaction。
    /// @errors context 未初始化、geometry/size 非法、backing 或 namespace OOM。
    pub(crate) fn prepare_virgl_resource(
        &self,
        create: VirglResourceCreate,
    ) -> Result<PreparedVirglResource<'_>, DrmError> {
        if create.width == 0
            || create.height == 0
            || create.depth == 0
            || create.array_size == 0
            || create.size == 0
        {
            return Err(DrmError::Invalid);
        }
        let stride = virgl_resource_stride(create)?;
        let context_id = self
            .state
            .lock()
            .context
            .as_ref()
            .map(|context| context.id)
            .ok_or(DrmError::Invalid)?;
        let handle = self
            .state
            .lock()
            .handle_ids
            .reserve()
            .map_err(reservation_error)?;
        let resource = match self.device.state.lock().graphics_resource_ids.reserve() {
            Ok(resource) => resource,
            Err(error) => {
                self.state.lock().handle_ids.rollback(handle);
                return Err(reservation_error(error));
            }
        };
        let handle_id = handle.id();
        let resource_id = resource.id();
        let size = usize::try_from(create.size).map_err(|_| DrmError::Invalid)?;
        let pages = size.div_ceil(PAGE_SIZE);
        let backing = Arc::try_new(
            DeviceBacking::try_allocate(pages, FrameAllocationClass::Reclaimable)
                .ok_or(DrmError::OutOfMemory)?,
        )
        .map_err(|_| DrmError::OutOfMemory)?;
        let buffer = Arc::try_new(VirglBuffer {
            resource_id,
            size,
            target: create.target,
            stride,
            width: create.width,
            height: create.height,
            backing,
            last_fence: Mutex::new(0),
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        let entry = FallibleMap::try_prepare(handle_id, buffer.clone())
            .map_err(|_| DrmError::OutOfMemory)?;
        Ok(PreparedVirglResource {
            file: self,
            handle: Some(handle),
            resource: Some(resource),
            handle_id,
            resource_id,
            context_id,
            create,
            buffer: Some(buffer),
            entry: Some(entry),
        })
    }

    /// @description 提交一个已验证 ownership 的 VirGL operation。
    /// @param command 不含 userspace pointer 的标准 operation。
    /// @return exact-fence wait 或 adapter-readiness retry token。
    /// @errors adapter failure 返回稳定 DRM error。
    pub(crate) fn submit_virgl(
        &self,
        command: VirglCommand<'_>,
    ) -> Result<DrmSubmission, DrmError> {
        let mut completion = self.device.completion.lock();
        let generation = completion.adapter_generation;
        let fence = match self.device.display.submit_virgl(command) {
            Ok(fence) => fence,
            Err(crate::drivers::DisplayError::WouldBlock) => {
                return Ok(DrmSubmission::Retry(DrmRetry {
                    device: self.device.clone(),
                    generation,
                }));
            }
            Err(error) => return Err(super::device::display_error(error)),
        };
        completion
            .timeline
            .submit(fence)
            .expect("published VirGL fence exceeded completion timeline");
        drop(completion);
        Ok(DrmSubmission::Wait(DrmWait {
            device: self.device.clone(),
            fence,
        }))
    }

    /// @description 查询本 OFD 的 VirGL resource metadata。
    pub(crate) fn virgl_resource_info(&self, handle: u32) -> Result<VirglResourceInfo, DrmError> {
        let state = self.state.lock();
        let buffer = state
            .graphics_buffers
            .get(&handle)
            .ok_or(DrmError::NotFound)?;
        Ok(VirglResourceInfo {
            handle,
            resource_id: buffer.resource_id,
            size: u32::try_from(buffer.size).map_err(|_| DrmError::Invalid)?,
        })
    }

    /// @description 原子撤销 handle 可见性并准备标准 GEM_CLOSE host lifecycle。
    /// @param handle 本 OFD 中待关闭的 VirGL GEM handle。
    /// @return 可等待最后 fence 并提交标准 RESOURCE_UNREF 的回滚事务。
    /// @errors handle 不存在或仍被 KMS framebuffer 引用。
    pub(crate) fn prepare_virgl_close(
        &self,
        handle: u32,
    ) -> Result<PreparedVirglClose<'_>, DrmError> {
        let mut state = self.state.lock();
        state.context.as_ref().ok_or(DrmError::Invalid)?;
        let buffer = state
            .graphics_buffers
            .get(&handle)
            .ok_or(DrmError::NotFound)?;
        if Arc::strong_count(buffer) != 1 {
            return Err(DrmError::Busy);
        }
        let resource_id = buffer.resource_id;
        let last_fence = *buffer.last_fence.lock();
        let entry = state
            .graphics_buffers
            .take_entry(&handle)
            .expect("checked VirGL handle disappeared");
        drop(state);
        Ok(PreparedVirglClose {
            file: self,
            entry: Some(entry),
            resource_id,
            last_fence,
        })
    }

    /// @description 返回一个 VirGL GEM handle 的 page-aligned mmap fake offset。
    pub(crate) fn map_virgl(&self, handle: u32) -> Result<u64, DrmError> {
        if handle == 0 || !self.state.lock().graphics_buffers.contains_key(&handle) {
            return Err(DrmError::NotFound);
        }
        Ok(u64::from(handle) << super::DUMB_OFFSET_SHIFT)
    }

    /// @description 为一个 resource 构造已验证的 host transfer command。
    /// @param transfer 已解码的 handle、方向、区域与 backing layout。
    /// @return 只含已验证 identity 和 geometry 的 adapter command。
    /// @errors context/handle 不存在，或 transfer 超出 backing/resource 返回 DRM error。
    pub(crate) fn transfer_command(
        &self,
        transfer: VirglTransfer,
    ) -> Result<VirglCommand<'static>, DrmError> {
        let VirglTransfer {
            handle,
            direction,
            offset,
            level,
            region,
            stride,
            layer_stride,
        } = transfer;
        let state = self.state.lock();
        let context_id = state.context.as_ref().ok_or(DrmError::Invalid)?.id;
        let buffer = state
            .graphics_buffers
            .get(&handle)
            .ok_or(DrmError::NotFound)?;
        let row_end = region
            .x
            .checked_add(region.width)
            .filter(|end| *end <= buffer.width);
        let column_end = region
            .y
            .checked_add(region.height)
            .filter(|end| *end <= buffer.height);
        let transfer_end = if buffer.target == PIPE_BUFFER {
            (region.y == 0
                && region.z == 0
                && region.height == 1
                && region.depth == 1
                && offset == region.x
                && (stride == 0 || stride == 1)
                && (layer_stride == 0 || layer_stride == buffer.width))
                .then(|| u64::from(offset).checked_add(u64::from(region.width)))
                .flatten()
        } else {
            let bytes_per_pixel = buffer.stride / buffer.width;
            let expected_offset = region
                .y
                .checked_mul(buffer.stride)
                .and_then(|row| row.checked_add(region.x.checked_mul(bytes_per_pixel)?));
            let expected_layer_stride = buffer.stride.checked_mul(buffer.height);
            if region.z == 0
                && region.depth == 1
                && stride == buffer.stride
                && Some(layer_stride) == expected_layer_stride
                && Some(offset) == expected_offset
            {
                u64::from(offset)
                    .checked_add(u64::from(region.height.saturating_sub(1)) * u64::from(stride))
                    .and_then(|start| {
                        start.checked_add(u64::from(region.width) * u64::from(bytes_per_pixel))
                    })
            } else {
                None
            }
        };
        if level != 0
            || region.depth != 1
            || region.width == 0
            || region.height == 0
            || row_end.is_none()
            || column_end.is_none()
            || transfer_end.is_none_or(|end| end > buffer.size as u64)
        {
            return Err(DrmError::Invalid);
        }
        Ok(VirglCommand::Transfer3d {
            direction,
            context_id,
            offset: u64::from(offset),
            resource_id: buffer.resource_id,
            level,
            region,
            stride,
            layer_stride,
        })
    }

    /// @description 验证 execbuffer resource set 并返回 file context identity。
    pub(crate) fn validate_exec_resources(&self, handles: &[u32]) -> Result<u32, DrmError> {
        let state = self.state.lock();
        let context = state.context.as_ref().ok_or(DrmError::Invalid)?;
        for handle in handles {
            if *handle == 0 || !state.graphics_buffers.contains_key(handle) {
                return Err(DrmError::NotFound);
            }
        }
        Ok(context.id)
    }

    /// @description 把一次已提交 fence 原子发布给所有引用的 GEM resource。
    pub(crate) fn record_virgl_fence(&self, handles: &[u32], fence: u64) -> Result<(), DrmError> {
        let state = self.state.lock();
        for handle in handles {
            let buffer = state
                .graphics_buffers
                .get(handle)
                .ok_or(DrmError::NotFound)?;
            *buffer.last_fence.lock() = fence;
        }
        Ok(())
    }

    /// @description 返回 handle 最近一次 exec/transfer fence 的 wait token。
    pub(crate) fn virgl_wait(&self, handle: u32) -> Result<Option<DrmWait>, DrmError> {
        let state = self.state.lock();
        let fence = *state
            .graphics_buffers
            .get(&handle)
            .ok_or(DrmError::NotFound)?
            .last_fence
            .lock();
        Ok((fence != 0).then(|| DrmWait {
            device: self.device.clone(),
            fence,
        }))
    }
}
