use alloc::sync::{Arc, Weak};
use spin::Mutex;

pub(crate) use crate::drivers::{DisplayRect, VirglBox, VirglCommand, VirglTransferDirection};
use crate::{
    drivers::{CursorCommand, DisplayMode, GraphicsDevice},
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeEnd},
    memory::{DeviceBacking, DeviceMappingSource, FrameAllocationClass, PAGE_SIZE},
};

const DUMB_OFFSET_SHIFT: u32 = 32;
pub(crate) const VIRGL_COMMAND_MAX: usize = 64 * 1024;

mod event;
pub(crate) use event::DrmEvent;
use event::{EVENT_QUEUE_CAPACITY, EventQueue};
include!("fence_timeline.rs");
mod card_file;
pub(crate) mod device;
mod graphics;
mod ioctl;
pub(crate) use graphics::VirglResourceCreate;
pub(crate) use graphics::VirglTransfer;
mod master;
mod mode;
mod publication;
mod publication_order;
pub(crate) use publication::{PreparedDumbBuffer, PreparedFramebuffer};
use publication_order::PublicationIdAllocator;

struct CompletionState {
    // OWNER: pending 同时绑定 adapter fence 与 scanout/damage/disable 领域结果；若拆分，
    // completion 与并发 RMFB/close 会把 active state 发布到错误 object。
    pending: Option<PendingDisplay>,
    active: Option<ActiveScanout>,
    // OWNER: timeline 按公开 submission 顺序登记 exact fence，并暂存乱序 completion；若直接
    // 取最大 fence，后完成的 command 会让尚未完成的旧 waiter 提前越过同步点。
    timeline: FenceTimeline,
    // OWNER: cursorq sequence 与 controlq fence 是独立命名空间；单独水位保证 cursor ioctl
    // 只等待 fast path completion，不会被耗时 scene render fence 阻塞或提前越过。
    cursor_completed: u64,
    // 每次 adapter transaction（含不改变 connector mode 的内部 display-info）
    // 完成时前进。同步 ioctl 用它等待 controlq 可再次提交；只观察 userspace fence
    // 会在 display-info 不产生 ModeChanged 时永久睡眠。
    adapter_generation: u64,
    // OWNER: sequence 只在成功完成一次 userspace scanout transaction 时前进；若按
    // submission 计数，adapter failure 或尚未生效的 framebuffer 会获得伪完成序号。
    sequence: u32,
    // close 在目标 flip 已进入 device 后不能撤销 descriptor；记录 OFD identity，最终
    // completion 到达后立即提交 fallback，避免关闭 fd 留下无 owner 的永久 scanout。
    reset_after_owner: Option<u64>,
}

struct PendingDisplay {
    fence: u64,
    operation: PendingOperation,
}

enum PendingOperation {
    Scanout {
        mode: DisplayMode,
        framebuffer: u32,
        owner: u64,
        event: Option<PendingEvent>,
    },
    Damage {
        owner: u64,
    },
    Release {
        owner: u64,
    },
    Disable,
}

struct PendingEvent {
    // Weak 避免 hardware pending transaction 反向保活已经 close 的 OFD；close 后完成
    // 仍推进 device fence，但不向不可达的 file queue 发布事件。
    file: Weak<DrmFile>,
    user_data: u64,
}

#[derive(Clone, Copy)]
struct ActiveScanout {
    framebuffer: u32,
    owner: u64,
    mode: DisplayMode,
}

struct DrmDeviceState {
    // OWNER: allocator 只回收 publication 前失败的 buffer identity；若仅保留 monotonic next，
    // 并发 transaction 的非尾部 copyout failure 会永久烧掉 identity。
    buffer_identities: PublicationIdAllocator<u64>,
    next_file_identity: u64,
    // OWNER: framebuffer allocator 与 device-wide object map 同锁；rollback storage 在 reserve
    // 时预留，copyout failure 可按任意并发顺序无分配回收未发布 ID。
    framebuffer_ids: PublicationIdAllocator<u32>,
    context_ids: PublicationIdAllocator<u32>,
    graphics_resource_ids: PublicationIdAllocator<u32>,
    // OWNER: 每个 context 创建时预分配 cleanup node；OFD Drop 只移动现有 AVL nodes，
    // 因而即使进程在 OOM/abort 路径退出也不会遗留 host VirGL resource/context。
    graphics_cleanups: FallibleMap<u32, graphics::VirglCleanup>,
    // OWNER: primary-node master identity 与 KMS object namespace 同属 device state；若放在
    // syscall 或 OFD flag，多个 open 会同时通过 modeset permission check。
    master: Option<u64>,
    // OWNER: connector preferred mode 独立于 completion.active CRTC mode；resize 只更新
    // 这里并发布 hotplug，不分配 framebuffer，也不隐式 modeset。
    mode: DisplayMode,
    // OWNER: framebuffer IDs 是 device-wide KMS object namespace；若放进 DrmFile，
    // GETRESOURCES 与另一个 primary-node open 会观察冲突或缺失的 mode object。
    framebuffers: FallibleMap<u32, Framebuffer>,
}

#[derive(Debug)]
struct DumbBuffer {
    identity: u64,
    pitch: u32,
    size: usize,
    backing: Arc<DeviceBacking>,
}

struct Framebuffer {
    owner: u64,
    width: u32,
    height: u32,
    pitch: u32,
    // OWNER: framebuffer object 独立保活 GEM backing；缺失该引用时 DESTROY_DUMB 会让
    // 已注册但尚未移除的 framebuffer 指向已回收 extent。
    backing: FramebufferBacking,
}

enum FramebufferBacking {
    Dumb(Arc<DumbBuffer>),
    Virgl(Arc<graphics::VirglBuffer>),
}

struct DrmFileState {
    // OWNER: handle allocator 与同 OFD map 共用 transaction lock；若独立递增，两个并发
    // CREATE_DUMB 可预留同一 handle，后提交者会覆盖前一个 object access。
    handle_ids: PublicationIdAllocator<u32>,
    // OWNER: buffers 是当前 OFD 唯一 GEM handle namespace；缺失 file-private collection
    // 会让不同 open 通过猜测 handle/offset 访问彼此 backing。
    buffers: FallibleMap<u32, Arc<DumbBuffer>>,
    // OWNER: context 与 graphics_buffers 属于同一 OFD；缺失同锁 ownership 会让 EXECBUFFER
    // 在 close/context teardown 间向已销毁的 host context 提交。
    context: Option<graphics::VirglContext>,
    graphics_buffers: FallibleMap<u32, Arc<graphics::VirglBuffer>>,
    // Linux 允许曾经的 master 在无当前 master 时重新取得 ownership；缺失该历史位会让
    // root-less display server 在 DROP_MASTER 后永久无法恢复。
    was_master: bool,
}

/// Linux DRM/KMS domain 的 primary display owner。
struct DrmDevice {
    display: Arc<dyn GraphicsDevice>,
    completion_read: Arc<PipeEnd>,
    completion_write: Arc<PipeEnd>,
    // OWNER: pending/completed fence 在同一锁下完成唯一状态迁移；若拆开，IRQ completion
    // 可在 waiter 读取之间丢失或被错误归属到后续 operation。
    completion: Mutex<CompletionState>,
    // OWNER: device-wide identity、file identity 与 framebuffer namespace 在同一状态 owner
    // 下发布；拆分会让 object ID publication 与 lookup/close cleanup 观察不同代际。
    state: Mutex<DrmDeviceState>,
}

/// 一个打开的 Linux DRM card OFD backend。
pub(crate) struct DrmFile {
    device: Arc<DrmDevice>,
    file_identity: u64,
    // OWNER: 每个 OFD 的 handle namespace 与 handle allocator 在同一锁内发布；若放到 device
    // global，两个独立 open 会错误地互相获得或销毁 buffer access。
    state: Mutex<DrmFileState>,
    // OWNER: 每个 OFD 唯一拥有 Linux event_space 与 read cursor。固定 4 KiB queue 让
    // deferred completion 永不分配；缺失独立 queue 会把一个 client 的事件泄漏给另一个。
    events: Mutex<EventQueue>,
}

/// DRM dumb-buffer 操作的稳定领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrmError {
    /// UAPI 参数、尺寸或 fake offset 非法。
    Invalid,
    /// 当前 OFD namespace 中没有目标 handle。
    NotFound,
    /// physical extent、Arc control block 或 map node 分配失败。
    OutOfMemory,
    /// monotonic identity 或 handle 空间耗尽。
    NoSpace,
    /// display adapter 已有未完成 transaction。
    Busy,
    /// display transport 或 response 损坏。
    Device,
    /// 当前 OFD 不是 KMS master，或无权重新取得 master。
    Permission,
}

/// RMFB transaction 的无分配进度结果。
pub(crate) enum FramebufferRemoval {
    /// object 已从 device namespace 删除。
    Removed,
    /// scanout disable 或 inactive RESOURCE_UNREF 尚未完成，caller 必须等待后重试删除。
    Wait(DrmWait),
    /// 另一个 display transaction 暂时占用 adapter，caller 必须等待 readiness 后重试。
    Retry(DrmRetry),
}

/// 一次 DRM operation 的同步提交结果。
pub(crate) enum DrmSubmission {
    /// operation 已发布；caller 必须等待其 exact fence。
    Wait(DrmWait),
    /// adapter 正由另一 transaction 占用；caller 必须等待 readiness 后重新提交。
    Retry(DrmRetry),
}

/// 一次硬件光标命令的同步提交结果。
pub(crate) enum DrmCursorSubmission {
    /// command 已发布；caller 必须等待 cursorq exact sequence。
    Wait(DrmCursorWait),
    /// cursorq 单 slot 正在使用；caller 等待 adapter generation 后重试。
    Retry(DrmRetry),
}

/// 一个不泄漏 adapter fence 编码的 DRM completion wait token。
pub(crate) struct DrmWait {
    device: Arc<DrmDevice>,
    fence: u64,
}

/// 一个保活光标 resource 直到 cursorq 已复制像素的 wait token。
pub(crate) struct DrmCursorWait {
    device: Arc<DrmDevice>,
    sequence: u64,
    // OWNER: UPDATE_CURSOR completion 前 QEMU 仍可能读取 2D resource backing；保活 dumb
    // buffer 使并发 DESTROY_DUMB 不会先回收 cursor pixels。
    _resource: Option<Arc<DumbBuffer>>,
}

/// 一个不泄漏 adapter transaction 的 DRM retry wait token。
pub(crate) struct DrmRetry {
    device: Arc<DrmDevice>,
    generation: u64,
}

impl DrmWait {
    /// 返回与该 wait token 绑定的 exact adapter fence。
    pub(crate) const fn fence(&self) -> u64 {
        self.fence
    }

    /// 排空旧 edge 并原子化地准备 scheduler wait。
    ///
    /// # Returns
    ///
    /// fence 已完成返回 None；否则返回统一 task registry 可等待的 Pipe source。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.device.completion.lock().timeline.completed() >= self.fence {
            return None;
        }
        self.device.completion_read.drain_readiness();
        (self.device.completion.lock().timeline.completed() < self.fence)
            .then(|| self.device.completion_read.pipe())
    }
}

impl DrmCursorWait {
    /// 排空旧 edge 并原子化地准备 cursorq completion wait。
    ///
    /// # Returns
    ///
    /// sequence 已完成返回 None；否则返回统一 task registry 可等待的 Pipe source。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.device.completion.lock().cursor_completed >= self.sequence {
            return None;
        }
        self.device.completion_read.drain_readiness();
        (self.device.completion.lock().cursor_completed < self.sequence)
            .then(|| self.device.completion_read.pipe())
    }
}

impl DrmRetry {
    /// 排空旧 edge 并原子化地准备 adapter-readiness wait。
    ///
    /// # Returns
    ///
    /// 取得 token 后已有 transaction 完成返回 None；否则返回统一 Pipe source。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.device.completion.lock().adapter_generation != self.generation {
            return None;
        }
        self.device.completion_read.drain_readiness();
        (self.device.completion.lock().adapter_generation == self.generation)
            .then(|| self.device.completion_read.pipe())
    }
}

/// `DRM_IOCTL_MODE_CREATE_DUMB` 的无 pointer 结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct DumbBufferInfo {
    /// 当前 OFD namespace 内的新 handle。
    pub(crate) handle: u32,
    /// 相邻 scanline 的字节距离。
    pub(crate) pitch: u32,
    /// page-aligned logical buffer size。
    pub(crate) size: u64,
}

/// legacy `DRM_IOCTL_MODE_GETFB` 的无 pointer 结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FramebufferInfo {
    /// framebuffer pixel width。
    pub(crate) width: u32,
    /// framebuffer pixel height。
    pub(crate) height: u32,
    /// linear scanline bytes。
    pub(crate) pitch: u32,
    /// 未建立 DRM master/CAP_SYS_ADMIN owner 时固定为零，避免泄漏 GEM handle。
    pub(crate) handle: u32,
}

/// Linux `drm_mode_modeinfo` 的领域投影，不包含 userspace pointer。
#[derive(Debug, Clone, Copy)]
pub(crate) struct DrmMode {
    pub(crate) clock: u32,
    pub(crate) hdisplay: u16,
    pub(crate) hsync_start: u16,
    pub(crate) hsync_end: u16,
    pub(crate) htotal: u16,
    pub(crate) vdisplay: u16,
    pub(crate) vsync_start: u16,
    pub(crate) vsync_end: u16,
    pub(crate) vtotal: u16,
    pub(crate) vrefresh: u32,
    pub(crate) flags: u32,
    pub(crate) mode_type: u32,
}

impl DrmFile {
    /// 通过独立 cursorq 切换 64x64 ARGB 光标；handle=None 表示隐藏。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD 中已经完成标准 2D host transfer 的 dumb handle。
    /// - `x`: scanout 0 水平位置。
    /// - `y`: scanout 0 垂直位置。
    /// - `hot_x`: resource 内水平热点。
    /// - `hot_y`: resource 内垂直热点。
    ///
    /// # Returns
    ///
    /// exact cursor completion wait 或 adapter readiness retry token。
    ///
    /// # Errors
    ///
    /// 非 master、CRTC inactive、resource/geometry 非法或 device failure。
    pub(crate) fn update_cursor(
        &self,
        handle: Option<u32>,
        x: u32,
        y: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result<DrmCursorSubmission, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let completion = self.device.completion.lock();
        if completion.active.is_none() {
            return Err(DrmError::Invalid);
        }
        let resource = handle
            .map(|handle| self.cursor_resource(handle))
            .transpose()?;
        if resource.is_some() && (hot_x >= 64 || hot_y >= 64)
            || resource.is_none() && (hot_x != 0 || hot_y != 0)
        {
            return Err(DrmError::Invalid);
        }
        let command = CursorCommand::Update {
            x,
            y,
            visible: resource.is_some(),
            hot_x,
            hot_y,
        };
        let generation = completion.adapter_generation;
        let result = self.device.display.submit_cursor(command);
        drop(completion);
        classify_cursor_submission(self, generation, resource, result)
    }

    /// 通过独立 cursorq 移动当前硬件光标，不读取或重绘 scene resource。
    ///
    /// # Parameters
    ///
    /// - `x`: scanout 0 水平位置。
    /// - `y`: scanout 0 垂直位置。
    ///
    /// # Returns
    ///
    /// adapter 接受最新位置后返回 unit，不等待 cursorq completion。
    ///
    /// # Errors
    ///
    /// 非 master、CRTC inactive 或 device failure。
    pub(crate) fn move_cursor(&self, x: u32, y: u32) -> Result<(), DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let completion = self.device.completion.lock();
        if completion.active.is_none() {
            return Err(DrmError::Invalid);
        }
        let result = self.device.display.move_cursor(x, y);
        drop(completion);
        result.map_err(device::display_error)
    }

    /// 准备 file-private XRGB8888 linear dumb buffer，不提前发布 handle。
    ///
    /// # Parameters
    ///
    /// - `width`: 非零 pixel width。
    /// - `height`: 非零 pixel height。
    /// - `bpp`: 仅支持标准 XRGB8888 color mode 32。
    /// - `flags`: Linux UAPI 要求为零。
    ///
    /// # Returns
    ///
    /// 已预留全部 fallible storage 的 publication transaction。
    ///
    /// # Errors
    ///
    /// 参数/溢出返回 Invalid；frame/control/node OOM 返回 OutOfMemory；identity/handle
    /// 耗尽返回 NoSpace。
    pub(crate) fn prepare_dumb(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
        flags: u32,
    ) -> Result<PreparedDumbBuffer<'_>, DrmError> {
        if width == 0 || height == 0 || bpp != 32 || flags != 0 {
            return Err(DrmError::Invalid);
        }
        let pitch = width.checked_mul(4).ok_or(DrmError::Invalid)?;
        let bytes = usize::try_from(pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(height as usize))
            .ok_or(DrmError::Invalid)?;
        let size = bytes
            .checked_add(PAGE_SIZE - 1)
            .map(|bytes| bytes / PAGE_SIZE * PAGE_SIZE)
            .filter(|size| *size != 0)
            .ok_or(DrmError::Invalid)?;

        // 1. 未发布 identity/handle 由 reservation token 独占；后续 OOM/copyout failure 会
        //    按任意并发顺序退回 allocator，已成功 publication 的 identity 仍绝不复用。
        let handle = publication::DumbHandleReservation::reserve(self)?;
        let identity = publication::BufferIdentityReservation::reserve(self)?;

        // 2. backing 与 Arc/node 全部在 handle publication 前分配；任一失败由 RAII 回收
        //    extent，file namespace 中不存在半初始化 GEM object。
        let backing =
            DeviceBacking::try_allocate(size / PAGE_SIZE, FrameAllocationClass::Reclaimable)
                .ok_or(DrmError::OutOfMemory)?;
        let backing = Arc::try_new(backing).map_err(|_| DrmError::OutOfMemory)?;
        let buffer = Arc::try_new(DumbBuffer {
            identity: identity.identity,
            pitch,
            size,
            backing,
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        let entry =
            FallibleMap::try_prepare(handle.handle, buffer).map_err(|_| DrmError::OutOfMemory)?;
        let info = DumbBufferInfo {
            handle: handle.handle,
            pitch,
            size: size as u64,
        };
        Ok(PreparedDumbBuffer::new(handle, identity, entry, info))
    }

    /// 为 file-private dumb handle 返回后续 mmap 使用的 fake byte offset。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD namespace 内的 GEM handle。
    ///
    /// # Returns
    ///
    /// handle 仍 live 时返回 page-aligned、非零且同 OFD 稳定的 offset。
    ///
    /// # Errors
    ///
    /// handle 不存在返回 NotFound。
    pub(crate) fn map_dumb(&self, handle: u32) -> Result<u64, DrmError> {
        let state = self.state.lock();
        if handle == 0 || !state.buffers.contains_key(&handle) {
            return Err(DrmError::NotFound);
        }
        Ok(u64::from(handle) << DUMB_OFFSET_SHIFT)
    }

    /// 删除 file-private GEM handle；已有 VMA 继续独立保活 backing。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD namespace 内的 handle。
    ///
    /// # Returns
    ///
    /// 删除成功返回 unit。
    ///
    /// # Errors
    ///
    /// handle 不存在返回 NotFound。
    pub(crate) fn destroy_dumb(&self, handle: u32) -> Result<(), DrmError> {
        let removed = self.state.lock().buffers.remove(&handle);
        let buffer = removed.ok_or(DrmError::NotFound)?;
        // DeviceBacking 的最后一个 Arc 会逐 extent 进入 buddy merge；必须在 GEM
        // namespace lock 外析构，否则回收会把 allocator lock 嵌套进 OFD transaction。
        drop(buffer);
        Ok(())
    }

    /// 解析 mmap fake offset，并把 object 引用转交给 VMA transaction。
    ///
    /// # Parameters
    ///
    /// - `offset`: `MAP_DUMB` 返回的 exact byte offset。
    /// - `length`: 请求映射的非零字节长度，不得超过 object logical size。
    ///
    /// # Returns
    ///
    /// 携带独立 Arc lifetime 与不可复用 identity 的 device mapping source。
    ///
    /// # Errors
    ///
    /// offset/length 非法返回 Invalid；object 已销毁返回 NotFound。
    pub(crate) fn mapping(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<DeviceMappingSource, DrmError> {
        let low_mask = (1u64 << DUMB_OFFSET_SHIFT) - 1;
        if length == 0 || offset & low_mask != 0 {
            return Err(DrmError::Invalid);
        }
        let handle = u32::try_from(offset >> DUMB_OFFSET_SHIFT)
            .ok()
            .filter(|handle| *handle != 0)
            .ok_or(DrmError::Invalid)?;
        let buffer = self.state.lock().buffers.get(&handle).cloned();
        if let Some(buffer) = buffer {
            if length > buffer.size {
                return Err(DrmError::Invalid);
            }
            return Ok(DeviceMappingSource::new(
                buffer.identity,
                buffer.backing.clone(),
            ));
        }
        let buffer = self
            .state
            .lock()
            .graphics_buffers
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)?;
        if length > buffer.size {
            return Err(DrmError::Invalid);
        }
        Ok(DeviceMappingSource::new(
            (1u64 << 63) | u64::from(buffer.resource_id),
            buffer.backing.clone(),
        ))
    }

    /// 准备 device-wide legacy framebuffer object，不提前发布 ID。
    ///
    /// # Parameters
    ///
    /// - `handle`: 当前 OFD 的 dumb handle。
    /// - `width`: framebuffer pixel width。
    /// - `height`: framebuffer pixel height。
    /// - `pitch`: linear scanline bytes，必须与 dumb allocation 一致。
    ///
    /// # Returns
    ///
    /// 已预留全部 fallible storage 的 publication transaction。
    ///
    /// # Errors
    ///
    /// handle/尺寸非法返回对应错误；ID/node 耗尽返回 NoSpace/OutOfMemory。
    pub(crate) fn prepare_framebuffer(
        &self,
        handle: u32,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> Result<PreparedFramebuffer<'_>, DrmError> {
        let backing = {
            let state = self.state.lock();
            if let Some(buffer) = state.buffers.get(&handle).cloned() {
                FramebufferBacking::Dumb(buffer)
            } else if let Some(buffer) = state.graphics_buffers.get(&handle).cloned() {
                FramebufferBacking::Virgl(buffer)
            } else {
                return Err(DrmError::NotFound);
            }
        };
        let (buffer_pitch, buffer_size, buffer_width, buffer_height) = match &backing {
            FramebufferBacking::Dumb(buffer) => (buffer.pitch, buffer.size, None, None),
            FramebufferBacking::Virgl(buffer) => (
                buffer.stride,
                buffer.size,
                Some(buffer.width),
                Some(buffer.height),
            ),
        };
        let required = usize::try_from(pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(height as usize))
            .filter(|required| *required <= buffer_size)
            .ok_or(DrmError::Invalid)?;
        if width == 0
            || height == 0
            || pitch != buffer_pitch
            || buffer_width.is_some_and(|buffer_width| buffer_width != width)
            || buffer_height.is_some_and(|buffer_height| buffer_height != height)
            || width.checked_mul(4).is_none_or(|minimum| pitch < minimum)
            || required == 0
        {
            return Err(DrmError::Invalid);
        }
        let id = publication::FramebufferIdReservation::reserve(self)?;
        let entry = FallibleMap::try_prepare(
            id.id,
            Framebuffer {
                owner: self.file_identity,
                width,
                height,
                pitch,
                backing,
            },
        )
        .map_err(|_| DrmError::OutOfMemory)?;
        Ok(PreparedFramebuffer::new(id, entry))
    }

    /// 返回当前 device-wide framebuffer object 数量。
    pub(crate) fn framebuffer_count(&self) -> usize {
        self.device.state.lock().framebuffers.len()
    }

    /// 按升序 index 读取一个 framebuffer ID，供 racy two-call KMS query 使用。
    ///
    /// # Parameters
    ///
    /// - `index`: 从零开始的 object index。
    ///
    /// # Returns
    ///
    /// 当前 snapshot 中对应 ID；并发增删导致越界返回 None。
    pub(crate) fn framebuffer_id(&self, index: usize) -> Option<u32> {
        self.device
            .state
            .lock()
            .framebuffers
            .iter()
            .nth(index)
            .map(|(&id, _)| id)
    }

    /// 查询本 OFD 创建的 legacy framebuffer metadata。
    ///
    /// # Parameters
    ///
    /// - `id`: device-wide framebuffer ID。
    ///
    /// # Returns
    ///
    /// owner 匹配时返回 metadata；未建模 master 权限时 handle 固定为零。
    ///
    /// # Errors
    ///
    /// object 不存在或属于其他 OFD 返回 NotFound。
    pub(crate) fn framebuffer(&self, id: u32) -> Result<FramebufferInfo, DrmError> {
        let (width, height, pitch) = {
            let state = self.device.state.lock();
            let framebuffer = state.framebuffers.get(&id).ok_or(DrmError::NotFound)?;
            if framebuffer.owner != self.file_identity {
                return Err(DrmError::NotFound);
            }
            (framebuffer.width, framebuffer.height, framebuffer.pitch)
        };
        Ok(FramebufferInfo {
            width,
            height,
            pitch,
            // LiteOS 尚无 DRM master/CAP_SYS_ADMIN owner；按 Linux GETFB 的非特权
            // disclosure boundary 返回零，不把 file-private GEM handle 泄漏给 query。
            handle: 0,
        })
    }

    /// 读取已经由 GPU completion 确认的 active framebuffer ID。
    ///
    /// # Returns
    ///
    /// 尚未由 userspace modeset 时返回 None；否则返回 device-wide object ID。
    pub(crate) fn active_framebuffer(&self) -> Option<u32> {
        self.device
            .completion
            .lock()
            .active
            .map(|active| active.framebuffer)
    }

    /// 异步提交一个本 OFD framebuffer 为固定 single-scanout backing。
    ///
    /// # Parameters
    ///
    /// - `id`: device-wide framebuffer object ID。
    ///
    /// # Returns
    ///
    /// 已提交 transaction 的 exact-fence wait token，或 adapter readiness retry token。
    ///
    /// # Errors
    ///
    /// object/尺寸非法、event queue 满或 adapter failure 返回稳定领域错误。
    pub(crate) fn page_flip(
        self: &Arc<Self>,
        id: u32,
        user_data: Option<u64>,
    ) -> Result<DrmSubmission, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        if user_data.is_some() && self.events.lock().len() == EVENT_QUEUE_CAPACITY {
            return Err(DrmError::Busy);
        }
        let event = user_data.map(|user_data| PendingEvent {
            file: Arc::downgrade(self),
            user_data,
        });
        let mode = completion
            .active
            .map(|active| active.mode)
            .ok_or(DrmError::Invalid)?;
        let generation = completion.adapter_generation;
        classify_submission(
            self,
            generation,
            self.submit_scanout(&mut completion, mode, id, event),
        )
    }

    /// 同步 modeset 到指定 framebuffer，不忙等 GPU completion。
    ///
    /// # Parameters
    ///
    /// - `id`: device-wide framebuffer object ID。
    ///
    /// # Returns
    ///
    /// 已提交 transaction 的 exact-fence wait token，或 adapter readiness retry token。
    ///
    /// # Errors
    ///
    /// object/尺寸/权限非法或 adapter failure 返回稳定领域错误。
    pub(crate) fn set_crtc(&self, id: u32, mode: DisplayMode) -> Result<DrmSubmission, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        let generation = completion.adapter_generation;
        classify_submission(
            self,
            generation,
            self.submit_scanout(&mut completion, mode, id, None),
        )
    }

    /// 同步把任一本 OFD framebuffer 的 dirty rectangles 传输到 resident resource。
    ///
    /// # Parameters
    ///
    /// - `id`: 属于本 OFD 的 framebuffer object ID；允许在 page flip 前同步 inactive buffer。
    /// - `rectangles`: 0..=32 个半开 scanout rectangle；零个表示 full framebuffer。
    ///
    /// # Returns
    ///
    /// Linux 语义下零 clips 扩展为 full framebuffer；返回 exact-fence wait token，
    /// 或 adapter readiness retry token。
    ///
    /// # Errors
    ///
    /// framebuffer 非本 OFD或 rectangle/device failure。
    pub(crate) fn dirty_framebuffer(
        &self,
        id: u32,
        rectangles: &[DisplayRect],
    ) -> Result<DrmSubmission, DrmError> {
        let mut completion = self.device.completion.lock();
        let generation = completion.adapter_generation;
        let mode = {
            let state = self.device.state.lock();
            let framebuffer = state.framebuffers.get(&id).ok_or(DrmError::NotFound)?;
            if framebuffer.owner != self.file_identity {
                return Err(DrmError::NotFound);
            }
            DisplayMode {
                width: framebuffer.width,
                height: framebuffer.height,
                pitch: framebuffer.pitch,
            }
        };
        let full = [DisplayRect {
            x: 0,
            y: 0,
            width: mode.width,
            height: mode.height,
        }];
        classify_submission(
            self,
            generation,
            self.submit_damage(
                &mut completion,
                id,
                if rectangles.is_empty() {
                    &full
                } else {
                    rectangles
                },
            ),
        )
    }

    /// 同步以 resource_id=0 禁用 scanout，并清除 active framebuffer state。
    ///
    /// # Returns
    ///
    /// 已提交 transaction 的 exact-fence wait token，或 adapter readiness retry token。
    ///
    /// # Errors
    ///
    /// permission 或 adapter failure 返回稳定领域错误。
    pub(crate) fn disable_crtc(&self) -> Result<DrmSubmission, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        let generation = completion.adapter_generation;
        classify_submission(self, generation, self.submit_disable(&mut completion))
    }
}

fn classify_submission(
    file: &DrmFile,
    generation: u64,
    result: Result<DrmWait, DrmError>,
) -> Result<DrmSubmission, DrmError> {
    match result {
        Ok(wait) => Ok(DrmSubmission::Wait(wait)),
        Err(DrmError::Busy) => Ok(DrmSubmission::Retry(DrmRetry {
            device: file.device.clone(),
            generation,
        })),
        Err(error) => Err(error),
    }
}

fn classify_cursor_submission(
    file: &DrmFile,
    generation: u64,
    resource: Option<Arc<DumbBuffer>>,
    result: Result<u64, crate::drivers::DisplayError>,
) -> Result<DrmCursorSubmission, DrmError> {
    match result {
        Ok(sequence) => Ok(DrmCursorSubmission::Wait(DrmCursorWait {
            device: file.device.clone(),
            sequence,
            _resource: resource,
        })),
        Err(crate::drivers::DisplayError::WouldBlock) => Ok(DrmCursorSubmission::Retry(DrmRetry {
            device: file.device.clone(),
            generation,
        })),
        Err(error) => Err(device::display_error(error)),
    }
}
