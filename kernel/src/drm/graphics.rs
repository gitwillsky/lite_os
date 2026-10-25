mod retirement;
pub(super) use retirement::{VirglCleanup, VirglCleanupAction, VirglCleanupCompletion};

use alloc::sync::Arc;
use alloc::vec::Vec;
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

/// `DRM_IOCTL_VIRTGPU_RESOURCE_CREATE` 的无 pointer 领域输入。
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

/// 一个 file-private VirGL GEM/resource 的稳定 UAPI metadata。
#[derive(Debug, Clone, Copy)]
pub(crate) struct VirglResourceInfo {
    pub(crate) handle: u32,
    pub(crate) resource_id: u32,
    pub(crate) size: u32,
}

/// 一次 file-private VirGL resource transfer 的已解码领域输入。
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
    /// 把 OFD-owned resource map 移入创建 context 时预分配的 cleanup node。
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

pub(super) struct VirglBuffer {
    pub(super) resource_id: u32,
    pub(super) size: usize,
    pub(super) target: u32,
    pub(super) stride: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) backing: Arc<DeviceBacking>,
    // OWNER: fence 与尚未使用的退休节点同锁；节点仅在 buffer=None 时保存在资源内，避免 Arc 自环。
    // 节点在 create 前预分配，使正常 GEM_CLOSE 无分配，OOM 不会阻断已发布资源的回收。
    pub(super) lifecycle: Mutex<VirglLifecycle>,
}

pub(super) struct VirglLifecycle {
    pub(super) fence: u64,
    retirement: Option<VacantEntry<u32, VirglRetirement>>,
}

/// 正常关闭或创建失败后的回收事务；预分配节点保活 backing。未发布 ID 只在 UNREF 完成后回收。
pub(super) struct VirglRetirement {
    pub(super) context_id: u32,
    pub(super) buffer: Option<Arc<VirglBuffer>>,
    pub(super) reservation: Option<UnpublishedId<u32>>,
    pub(super) in_flight: Option<u64>,
}

/// 一次提交从 handle 验证到 fence 发布期间的资源强引用；不能只验证 ID 后丢掉 ownership。
pub(super) struct VirglAccess {
    pub(super) context_id: u32,
    buffers: Vec<Arc<VirglBuffer>>,
}
impl VirglAccess {
    /// host 接受命令后发布 fence；并发提交可能逆序登记，不能降低最后使用水位。
    pub(super) fn record(&self, fence: u64) {
        for buffer in &self.buffers {
            let mut lifecycle = buffer.lifecycle.lock();
            lifecycle.fence = lifecycle.fence.max(fence);
        }
    }
}

/// 尚未发布、但已预留 device context identity 的 transaction。
pub(crate) struct PreparedVirglContext<'file> {
    file: &'file DrmFile,
    _admission: crate::sync::TaskMutexGuard<'file, ()>,
    reservation: Option<UnpublishedId<u32>>,
    id: u32,
    capset_id: u32,
    name: [u8; 64],
    name_length: usize,
    cleanup: Option<VacantEntry<u32, VirglCleanup>>,
}

impl PreparedVirglContext<'_> {
    /// 返回应提交给 adapter 的标准 CTX_CREATE operation。
    ///
    /// # Returns
    ///
    /// context identity、capset init 与 debug name 全部绑定的 command。
    pub(crate) fn command(&self) -> VirglCommand<'_> {
        VirglCommand::ContextCreate {
            context_id: self.id,
            context_init: self.capset_id,
            name: &self.name[..self.name_length],
        }
    }

    /// 在 CTX_CREATE 已被 host 队列接受后发布 context；后续命令经同一队列排序，退出交给 completion 回收。
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

/// 已完成 backing、handle/resource identity 与 map node 预留的 transaction。
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
    // OWNER: None 表示 host 尚未接受 create；首次提交后只能在 UNREF completion 后回收 resource ID。
    // 缺失时 EINTR/OOM/copyout fault 会提前复用仍被 host 使用的 identity 或释放 DMA backing。
    submitted: bool,
    cleanup: Option<VacantEntry<u32, VirglRetirement>>,
}

/// 已从 handle namespace 摘除的 resource；publish 后由 exact-fence 回收队列独占。
/// 未发布时 Drop 用原 AVL node 无分配恢复 handle，不对 host 提交 UNREF。
pub(crate) struct PreparedVirglClose<'file> {
    file: &'file DrmFile,
    entry: Option<VacantEntry<u32, Arc<VirglBuffer>>>,
}
impl PreparedVirglClose<'_> {
    /// 原子退休 handle；异步 UNREF 完成前保活 backing，不把可中断 wait 放在销毁命令之后。
    pub(crate) fn publish(mut self) {
        let buffer = self
            .entry
            .take()
            .expect("VirGL close lost handle entry")
            .into_value();
        let mut cleanup = buffer
            .lifecycle
            .lock()
            .retirement
            .take()
            .expect("published resource lost retirement node");
        cleanup.value_mut().buffer = Some(buffer);
        self.file.retire_virgl_resource(cleanup);
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
    /// host 接受命令后、开始可中断等待前记录 exact fence；不得在等待成功后才登记。
    pub(crate) fn record_submission(&mut self, fence: u64) {
        self.submitted = true;
        self.buffer
            .as_ref()
            .expect("prepared resource missing buffer")
            .lifecycle
            .lock()
            .fence = fence;
    }
    /// 返回 host resource create operation。
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

    /// 返回 guest backing attach operation。
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

    /// 返回 context/resource ownership attach operation。
    pub(crate) fn context_attach_command(&self) -> VirglCommand<'static> {
        VirglCommand::ContextAttachResource {
            context_id: self.context_id,
            resource_id: self.resource_id,
        }
    }

    /// 返回 UAPI copyout 所需的 stable identities。
    pub(crate) fn info(&self) -> VirglResourceInfo {
        VirglResourceInfo {
            handle: self.handle_id,
            resource_id: self.resource_id,
            size: self.create.size as u32,
        }
    }

    /// 在三个 host operation 与 UAPI copyout 全部成功后发布 GEM handle。
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
        let buffer = self.buffer.take().expect("VirGL buffer missing");
        let mut cleanup = self
            .cleanup
            .take()
            .expect("published resource lost retirement node");
        drop(cleanup.value_mut().buffer.take());
        buffer.lifecycle.lock().retirement = Some(cleanup);
        drop(buffer);
        self.file.state.lock().graphics_buffers.commit_vacant(entry);
        self.file.state.lock().handle_ids.publish(handle);
        self.file
            .device
            .state
            .lock()
            .graphics_resource_ids
            .publish(resource);
    }
}

impl Drop for PreparedVirglResource<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.file.state.lock().handle_ids.rollback(handle);
        }
        if let Some(resource) = self.resource.take() {
            if self.submitted {
                let mut cleanup = self
                    .cleanup
                    .take()
                    .expect("submitted resource lost cleanup node");
                cleanup.value_mut().reservation = Some(resource);
                drop(self.entry.take());
                drop(self.buffer.take());
                self.file.retire_virgl_resource(cleanup);
            } else {
                self.file
                    .device
                    .state
                    .lock()
                    .graphics_resource_ids
                    .rollback(resource);
            }
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
    /// 取得并保活一个满足 VirtIO-GPU cursorq 固定 geometry 的 dumb buffer。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD 的标准 DRM dumb GEM handle。
    ///
    /// # Returns
    ///
    /// 64x64x4 guest-backed pixel owner。
    ///
    /// # Errors
    ///
    /// handle 不存在或 resource contract 不匹配。
    pub(super) fn cursor_resource(&self, handle: u32) -> Result<Arc<DumbBuffer>, DrmError> {
        let state = self.state.lock();
        let buffer = state.buffers.get(&handle).ok_or(DrmError::NotFound)?;
        if buffer.pitch != 64 * 4 || buffer.size != 64 * 64 * 4 {
            return Err(DrmError::Invalid);
        }
        Ok(buffer.clone())
    }

    /// 把标准 dumb cursor pixels 上传到 adapter-owned 2D resource。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD 的 64x64x4 dumb GEM handle。
    ///
    /// # Returns
    ///
    /// 完整 2D CREATE/ATTACH/TRANSFER fence，或 adapter readiness retry。
    ///
    /// # Errors
    ///
    /// 非 master、CRTC inactive、handle/geometry 非法或 device failure。
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

    /// 返回 host 固定的 VirGL capset ABI。
    pub(crate) fn virgl_capset_info(&self) -> Result<VirglCapsetInfo, DrmError> {
        self.device
            .display
            .virgl_capset_info()
            .ok_or(DrmError::Device)
    }

    /// 复制 immutable VirGL capset bytes。
    pub(crate) fn copy_virgl_capset(&self, output: &mut [u8]) -> Result<usize, DrmError> {
        self.device
            .display
            .copy_virgl_capset(output)
            .map_err(super::device::display_error)
    }

    /// 返回此 adapter 是否支持显式 VirGL context 参数。
    pub(crate) fn supports_virgl_context_init(&self) -> bool {
        self.device.display.supports_virgl_context_init()
    }

    /// 预留本 OFD 的唯一 VirGL context。
    ///
    /// # Parameters
    ///
    /// - `capset_id`: 必须等于 adapter 选中的 VirGL capset。
    /// - `name`: 最多 64-byte explicit debug name。
    ///
    /// # Returns
    ///
    /// 尚未发布、可先提交 CTX_CREATE 的 transaction。
    ///
    /// # Errors
    ///
    /// 重复初始化、capset 不匹配、名称过长或 identity OOM/耗尽。
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
        let admission = self
            .context_creation
            .lock()
            .map_err(|_| DrmError::OutOfMemory)?;
        self.prepare_context(capset_id, name, admission)
    }

    /// 按 Linux virtio-gpu legacy 语义准备首个 3D resource 的惰性 context。
    ///
    /// # Returns
    ///
    /// context 已存在时为 None；否则返回 `context_init=0` 的 CTX_CREATE transaction。
    pub(crate) fn prepare_legacy_virgl_context(
        &self,
    ) -> Result<Option<PreparedVirglContext<'_>>, DrmError> {
        self.virgl_capset_info()?;
        let admission = self
            .context_creation
            .lock()
            .map_err(|_| DrmError::OutOfMemory)?;
        let state = self.state.lock();
        if state.context.is_some() {
            return Ok(None);
        }
        if !state.graphics_buffers.is_empty() {
            return Err(DrmError::Invalid);
        }
        drop(state);
        self.prepare_context(0, &[], admission).map(Some)
    }

    fn prepare_context<'file>(
        &'file self,
        capset_id: u32,
        name: &[u8],
        admission: crate::sync::TaskMutexGuard<'file, ()>,
    ) -> Result<PreparedVirglContext<'file>, DrmError> {
        if self.state.lock().context.is_some() {
            return Err(DrmError::AlreadyExists);
        }
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
            _admission: admission,
            reservation: Some(reservation),
            id,
            capset_id,
            name: encoded_name,
            name_length: name.len(),
            cleanup: Some(cleanup),
        })
    }

    /// 预留一个标准 VirGL resource 与 file-private GEM handle。
    ///
    /// # Parameters
    ///
    /// - `create`: 已从 Linux UAPI 解码的 resource geometry/format/storage contract。
    ///
    /// # Returns
    ///
    /// host create/attach 完成前不可查询的 prepared transaction。
    ///
    /// # Errors
    ///
    /// context 未初始化、geometry/size 非法、backing 或 namespace OOM。
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
        // 1. identity 预留后立即交给现有 RAII transaction。
        // 2. 后续 backing/Arc/map node 任一分配失败，Drop 无分配回收两个 identity。
        // 不得在建立 transaction 前使用 ?，否则 OOM 会永久烧掉未发布的 handle/resource ID。
        let mut prepared = PreparedVirglResource {
            file: self,
            handle: Some(handle),
            resource: Some(resource),
            handle_id,
            resource_id,
            context_id,
            create,
            buffer: None,
            entry: None,
            submitted: false,
            cleanup: None,
        };
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
            lifecycle: Mutex::new(VirglLifecycle {
                fence: 0,
                retirement: None,
            }),
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        prepared.buffer = Some(buffer.clone());
        prepared.entry = Some(
            FallibleMap::try_prepare(handle_id, buffer.clone())
                .map_err(|_| DrmError::OutOfMemory)?,
        );
        prepared.cleanup = Some(
            FallibleMap::try_prepare(
                resource_id,
                VirglRetirement {
                    context_id,
                    buffer: Some(buffer),
                    reservation: None,
                    in_flight: None,
                },
            )
            .map_err(|_| DrmError::OutOfMemory)?,
        );
        Ok(prepared)
    }

    /// 提交一个已验证 ownership 的 VirGL operation。
    ///
    /// # Parameters
    ///
    /// - `command`: 不含 userspace pointer 的标准 operation。
    ///
    /// # Returns
    ///
    /// exact-fence wait 或 adapter-readiness retry token。
    ///
    /// # Errors
    ///
    /// adapter failure 返回稳定 DRM error。
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

    /// 查询本 OFD 的 VirGL resource metadata。
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

    /// 原子撤销 handle 可见性并准备标准 GEM_CLOSE host lifecycle。
    ///
    /// # Parameters
    ///
    /// - `handle`: 本 OFD 中待关闭的 VirGL GEM handle。
    ///
    /// # Returns
    ///
    /// 可原子退休 handle、按最后 fence 异步提交 RESOURCE_UNREF 的回滚事务。
    ///
    /// # Errors
    ///
    /// handle/context 不存在；KMS/提交引用由退休队列保活，不阻止关闭 handle。
    pub(crate) fn prepare_virgl_close(
        &self,
        handle: u32,
    ) -> Result<PreparedVirglClose<'_>, DrmError> {
        let mut state = self.state.lock();
        state.context.as_ref().ok_or(DrmError::Invalid)?;
        let entry = state
            .graphics_buffers
            .take_entry(&handle)
            .ok_or(DrmError::NotFound)?;
        drop(state);
        Ok(PreparedVirglClose {
            file: self,
            entry: Some(entry),
        })
    }

    fn retire_virgl_resource(&self, cleanup: VacantEntry<u32, VirglRetirement>) {
        self.device
            .state
            .lock()
            .graphics_retirements
            .commit_vacant(cleanup);
        let mut completion = self.device.completion.lock();
        super::device::advance_graphics_cleanup(&self.device, &mut completion);
    }

    /// ioctl 的所有临时资源 pin 退出后推进退休；失败的 framebuffer publication 也必须经过此出口。
    pub(super) fn progress_retirements(&self) {
        if self.device.state.lock().graphics_retirements.is_empty() {
            return;
        }
        let mut completion = self.device.completion.lock();
        super::device::advance_graphics_cleanup(&self.device, &mut completion);
    }

    /// 返回一个 VirGL GEM handle 的 page-aligned mmap fake offset。
    pub(crate) fn map_virgl(&self, handle: u32) -> Result<u64, DrmError> {
        if handle == 0 || !self.state.lock().graphics_buffers.contains_key(&handle) {
            return Err(DrmError::NotFound);
        }
        Ok(u64::from(handle) << super::DUMB_OFFSET_SHIFT)
    }

    /// 为一个 resource 构造已验证的 host transfer command。
    ///
    /// # Parameters
    ///
    /// - `transfer`: 已解码的 handle、方向、区域与 backing layout。
    ///
    /// # Returns
    ///
    /// 只含已验证 identity 和 geometry 的 adapter command。
    ///
    /// # Errors
    ///
    /// context/handle 不存在，或 transfer 超出 backing/resource 返回 DRM error。
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

    /// 验证并保活 exec/transfer resource set，直到调用方发布已接受命令的 fence。
    /// 数组存储先在 spin lock 外预留；无效 handle 或 OOM 不提交 host 命令。
    pub(super) fn access_virgl_resources(
        &self,
        handles: impl ExactSizeIterator<Item = u32>,
    ) -> Result<VirglAccess, DrmError> {
        let mut buffers = Vec::new();
        buffers
            .try_reserve_exact(handles.len())
            .map_err(|_| DrmError::OutOfMemory)?;
        let state = self.state.lock();
        let context = state.context.as_ref().ok_or(DrmError::Invalid)?;
        for handle in handles {
            buffers.push(
                state
                    .graphics_buffers
                    .get(&handle)
                    .ok_or(DrmError::NotFound)?
                    .clone(),
            );
        }
        Ok(VirglAccess {
            context_id: context.id,
            buffers,
        })
    }

    /// 返回 handle 最近一次 exec/transfer fence 的 wait token。
    pub(crate) fn virgl_wait(&self, handle: u32) -> Result<Option<DrmWait>, DrmError> {
        let state = self.state.lock();
        let fence = state
            .graphics_buffers
            .get(&handle)
            .ok_or(DrmError::NotFound)?
            .lifecycle
            .lock()
            .fence;
        Ok((fence != 0).then(|| DrmWait {
            device: self.device.clone(),
            fence,
        }))
    }
}
