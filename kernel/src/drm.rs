use alloc::sync::{Arc, Weak};
use spin::Mutex;

use crate::{
    drivers::{DisplayDevice, DisplayMode},
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeEnd},
    memory::{
        DeviceMappingSource, FrameAllocationClass, FrameTracker, PAGE_SIZE, alloc_contiguous,
    },
};

const DUMB_OFFSET_SHIFT: u32 = 32;

mod event;
pub(crate) use event::DrmEvent;
use event::{EVENT_QUEUE_CAPACITY, EventQueue};
mod master;
mod mode;
use mode::cvt_mode;
pub(crate) mod device;

struct CompletionState {
    // OWNER: pending 同时绑定 adapter operation fence 与目标 framebuffer；若拆分，IRQ
    // completion 与并发 RMFB/page-flip 会把 active state 发布到错误 object。
    pending: Option<PendingScanout>,
    active: Option<ActiveScanout>,
    // completed 单调前进，waiter 以 `>= fence` 判断；若只保存一次 edge，旧 Pipe token
    // 被其他 waiter 排空后会永久丢失已完成事实。
    completed: u64,
    // OWNER: sequence 只在成功完成一次 userspace scanout transaction 时前进；若按
    // submission 计数，adapter failure 或尚未生效的 framebuffer 会获得伪完成序号。
    sequence: u32,
    // close 在目标 flip 已进入 device 后不能撤销 descriptor；记录 OFD identity，最终
    // completion 到达后立即提交 fallback，避免关闭 fd 留下无 owner 的永久 scanout。
    reset_after_owner: Option<u64>,
}

struct PendingScanout {
    fence: u64,
    framebuffer: Option<u32>,
    owner: Option<u64>,
    event: Option<PendingEvent>,
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
}

struct DrmDeviceState {
    next_buffer_identity: u64,
    next_file_identity: u64,
    next_framebuffer_id: u32,
    // OWNER: primary-node master identity 与 KMS object namespace 同属 device state；若放在
    // syscall 或 OFD flag，多个 open 会同时通过 modeset permission check。
    master: Option<u64>,
    // OWNER: framebuffer IDs 是 device-wide KMS object namespace；若放进 DrmFile，
    // GETRESOURCES 与另一个 primary-node open 会观察冲突或缺失的 mode object。
    framebuffers: FallibleMap<u32, Framebuffer>,
}

#[derive(Debug)]
struct DumbBuffer {
    identity: u64,
    pitch: u32,
    size: usize,
    backing: Arc<FrameTracker>,
}

struct Framebuffer {
    owner: u64,
    width: u32,
    height: u32,
    pitch: u32,
    // OWNER: framebuffer object 独立保活 GEM backing；缺失该引用时 DESTROY_DUMB 会让
    // 已注册但尚未移除的 framebuffer 指向已回收 extent。
    buffer: Arc<DumbBuffer>,
}

struct DrmFileState {
    // OWNER: handle allocator 与同 OFD map 共用 transaction lock；若独立递增，两个并发
    // CREATE_DUMB 可预留同一 handle，后提交者会覆盖前一个 object access。
    next_handle: u32,
    // OWNER: buffers 是当前 OFD 唯一 GEM handle namespace；缺失 file-private collection
    // 会让不同 open 通过猜测 handle/offset 访问彼此 backing。
    buffers: FallibleMap<u32, Arc<DumbBuffer>>,
    // Linux 允许曾经的 master 在无当前 master 时重新取得 ownership；缺失该历史位会让
    // root-less display server 在 DROP_MASTER 后永久无法恢复。
    was_master: bool,
}

/// @description Linux DRM/KMS domain 的 primary display owner。
struct DrmDevice {
    display: Arc<dyn DisplayDevice>,
    _mode: DisplayMode,
    // OWNER: fallback backing 独立于 adapter 当前 active resource 永久保活；若只依赖
    // active owner，首次 userspace modeset 的 UNREF 会让 close/disable 无法恢复黑屏。
    fallback_backing: Arc<FrameTracker>,
    completion_read: Arc<PipeEnd>,
    completion_write: Arc<PipeEnd>,
    // OWNER: pending/completed fence 在同一锁下完成唯一状态迁移；若拆开，IRQ completion
    // 可在 waiter 读取之间丢失或被错误归属到后续 operation。
    completion: Mutex<CompletionState>,
    // OWNER: device-wide identity、file identity 与 framebuffer namespace 在同一状态 owner
    // 下发布；拆分会让 object ID publication 与 lookup/close cleanup 观察不同代际。
    state: Mutex<DrmDeviceState>,
}

/// @description 一个打开的 Linux DRM card OFD backend。
pub(crate) struct DrmFile {
    device: Arc<DrmDevice>,
    file_identity: u64,
    // OWNER: 每个 OFD 的 handle namespace 与 next_handle 在同一锁内发布；若放到 device
    // global，两个独立 open 会错误地互相获得或销毁 buffer access。
    state: Mutex<DrmFileState>,
    // OWNER: 每个 OFD 唯一拥有 Linux event_space 与 read cursor。固定 4 KiB queue 让
    // deferred completion 永不分配；缺失独立 queue 会把一个 client 的事件泄漏给另一个。
    events: Mutex<EventQueue>,
}

/// @description DRM dumb-buffer 操作的稳定领域错误。
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

/// @description RMFB transaction 的无分配进度结果。
pub(crate) enum FramebufferRemoval {
    /// object 已从 device namespace 删除。
    Removed,
    /// active object 已切换到 fallback，caller 必须等待后重试删除。
    Wait(DrmWait),
}

/// @description 一个不泄漏 adapter fence 编码的 DRM completion wait token。
pub(crate) struct DrmWait {
    device: Arc<DrmDevice>,
    fence: u64,
}

impl DrmWait {
    /// @description 排空旧 edge 并原子化地准备 scheduler wait。
    /// @return fence 已完成返回 None；否则返回统一 task registry 可等待的 Pipe source。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.device.completion.lock().completed >= self.fence {
            return None;
        }
        self.device.completion_read.drain_readiness();
        (self.device.completion.lock().completed < self.fence)
            .then(|| self.device.completion_read.pipe())
    }
}

/// @description `DRM_IOCTL_MODE_CREATE_DUMB` 的无 pointer 结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct DumbBufferInfo {
    /// 当前 OFD namespace 内的新 handle。
    pub(crate) handle: u32,
    /// 相邻 scanline 的字节距离。
    pub(crate) pitch: u32,
    /// page-aligned logical buffer size。
    pub(crate) size: u64,
}

/// @description legacy `DRM_IOCTL_MODE_GETFB` 的无 pointer 结果。
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

/// @description Linux `drm_mode_modeinfo` 的领域投影，不包含 userspace pointer。
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
    /// @description 读取 immutable single-connector preferred mode。
    /// @return 与 VirtIO display-info resolution 对应的 Linux CVT 60 Hz mode。
    pub(crate) fn mode(&self) -> DrmMode {
        cvt_mode(self.device._mode)
    }

    /// @description 创建 file-private XRGB8888 linear dumb buffer handle。
    ///
    /// @param width 非零 pixel width。
    /// @param height 非零 pixel height。
    /// @param bpp 仅支持标准 XRGB8888 color mode 32。
    /// @param flags Linux UAPI 要求为零。
    /// @return 新 handle、linear pitch 与 page-aligned logical size。
    /// @errors 参数/溢出返回 Invalid；frame/control/node OOM 返回 OutOfMemory；identity/handle
    /// 耗尽返回 NoSpace。
    pub(crate) fn create_dumb(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
        flags: u32,
    ) -> Result<DumbBufferInfo, DrmError> {
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

        // 1. identity/handle 只做单调预留且不跨 allocation 持锁；失败允许留下 hole，
        //    但绝不复用已向 futex/mmap publication 暴露过的 identity。
        let identity = {
            let mut state = self.device.state.lock();
            let identity = state.next_buffer_identity;
            state.next_buffer_identity = identity.checked_add(1).ok_or(DrmError::NoSpace)?;
            identity
        };
        let handle = {
            let mut state = self.state.lock();
            let handle = state.next_handle;
            state.next_handle = handle.checked_add(1).ok_or(DrmError::NoSpace)?;
            handle
        };

        // 2. backing 与 Arc/node 全部在 handle publication 前分配；任一失败由 RAII 回收
        //    extent，file namespace 中不存在半初始化 GEM object。
        let backing = alloc_contiguous(size / PAGE_SIZE, FrameAllocationClass::Reclaimable)
            .ok_or(DrmError::OutOfMemory)?;
        let backing = Arc::try_new(backing).map_err(|_| DrmError::OutOfMemory)?;
        let buffer = Arc::try_new(DumbBuffer {
            identity,
            pitch,
            size,
            backing,
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        let prepared =
            FallibleMap::try_prepare(handle, buffer).map_err(|_| DrmError::OutOfMemory)?;

        // 3. handle 在唯一 OFD lock 下原子可见；mmap/DESTROY 只能观察完整 object。
        self.state.lock().buffers.commit_vacant(prepared);
        Ok(DumbBufferInfo {
            handle,
            pitch,
            size: size as u64,
        })
    }

    /// @description 为 file-private dumb handle 返回后续 mmap 使用的 fake byte offset。
    /// @param handle 当前 OFD namespace 内的 GEM handle。
    /// @return handle 仍 live 时返回 page-aligned、非零且同 OFD 稳定的 offset。
    /// @errors handle 不存在返回 NotFound。
    pub(crate) fn map_dumb(&self, handle: u32) -> Result<u64, DrmError> {
        if handle == 0 || !self.state.lock().buffers.contains_key(&handle) {
            return Err(DrmError::NotFound);
        }
        Ok(u64::from(handle) << DUMB_OFFSET_SHIFT)
    }

    /// @description 删除 file-private GEM handle；已有 VMA 继续独立保活 backing。
    /// @param handle 当前 OFD namespace 内的 handle。
    /// @return 删除成功返回 unit。
    /// @errors handle 不存在返回 NotFound。
    pub(crate) fn destroy_dumb(&self, handle: u32) -> Result<(), DrmError> {
        let removed = self.state.lock().buffers.remove(&handle);
        let buffer = removed.ok_or(DrmError::NotFound)?;
        // FrameTracker 的最后一个 Arc 会进入 buddy merge；必须在 GEM namespace lock
        // 外析构，否则大 extent 回收会把 allocator lock 嵌套进 OFD transaction lock。
        drop(buffer);
        Ok(())
    }

    /// @description 解析 mmap fake offset，并把 object 引用转交给 VMA transaction。
    ///
    /// @param offset `MAP_DUMB` 返回的 exact byte offset。
    /// @param length 请求映射的非零字节长度，不得超过 object logical size。
    /// @return 携带独立 Arc lifetime 与不可复用 identity 的 device mapping source。
    /// @errors offset/length 非法返回 Invalid；object 已销毁返回 NotFound。
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
        let buffer = self
            .state
            .lock()
            .buffers
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)?;
        if length > buffer.size {
            return Err(DrmError::Invalid);
        }
        Ok(DeviceMappingSource::new(
            buffer.identity,
            buffer.backing.clone(),
        ))
    }

    /// @description 从 file-private dumb handle 创建 device-wide legacy framebuffer object。
    ///
    /// @param handle 当前 OFD 的 dumb handle。
    /// @param width framebuffer pixel width。
    /// @param height framebuffer pixel height。
    /// @param pitch linear scanline bytes，必须与 dumb allocation 一致。
    /// @return 新的 device-wide framebuffer object ID。
    /// @errors handle/尺寸非法返回对应错误；ID/node 耗尽返回 NoSpace/OutOfMemory。
    pub(crate) fn add_framebuffer(
        &self,
        handle: u32,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> Result<u32, DrmError> {
        let buffer = self
            .state
            .lock()
            .buffers
            .get(&handle)
            .cloned()
            .ok_or(DrmError::NotFound)?;
        let required = usize::try_from(pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(height as usize))
            .filter(|required| *required <= buffer.size)
            .ok_or(DrmError::Invalid)?;
        if width == 0
            || height == 0
            || pitch != buffer.pitch
            || width.checked_mul(4).is_none_or(|minimum| pitch < minimum)
            || required == 0
        {
            return Err(DrmError::Invalid);
        }
        let id = {
            let mut state = self.device.state.lock();
            let id = state.next_framebuffer_id;
            state.next_framebuffer_id = id.checked_add(1).ok_or(DrmError::NoSpace)?;
            id
        };
        let prepared = FallibleMap::try_prepare(
            id,
            Framebuffer {
                owner: self.file_identity,
                width,
                height,
                pitch,
                buffer,
            },
        )
        .map_err(|_| DrmError::OutOfMemory)?;
        self.device
            .state
            .lock()
            .framebuffers
            .commit_vacant(prepared);
        Ok(id)
    }

    /// @description 删除本 OFD 创建的 framebuffer object。
    /// @param id device-wide framebuffer ID。
    /// @return object 已删除，或 active scanout fallback transaction 的 wait token。
    /// @errors object 不存在返回 NotFound；并发 flip 或 adapter/wait 失败返回对应错误。
    pub(crate) fn remove_framebuffer(&self, id: u32) -> Result<FramebufferRemoval, DrmError> {
        let mut completion = self.device.completion.lock();
        if completion
            .pending
            .as_ref()
            .is_some_and(|pending| pending.framebuffer == Some(id))
        {
            return Err(DrmError::Busy);
        }
        if completion
            .active
            .is_some_and(|active| active.framebuffer == id)
        {
            return self
                .submit_scanout(&mut completion, None, None)
                .map(FramebufferRemoval::Wait);
        }
        let removed = {
            let mut state = self.device.state.lock();
            if state
                .framebuffers
                .get(&id)
                .is_none_or(|framebuffer| framebuffer.owner != self.file_identity)
            {
                return Err(DrmError::NotFound);
            }
            state
                .framebuffers
                .remove(&id)
                .expect("validated framebuffer disappeared under owner lock")
        };
        drop(completion);
        // framebuffer 可能持有 GEM backing 的最后一个 Arc；页回收不得发生在 device
        // object namespace lock 内，否则 close/RMFB 会放大所有 KMS query 的尾延迟。
        drop(removed);
        Ok(FramebufferRemoval::Removed)
    }

    /// @description 返回当前 device-wide framebuffer object 数量。
    pub(crate) fn framebuffer_count(&self) -> usize {
        self.device.state.lock().framebuffers.len()
    }

    /// @description 按升序 index 读取一个 framebuffer ID，供 racy two-call KMS query 使用。
    /// @param index 从零开始的 object index。
    /// @return 当前 snapshot 中对应 ID；并发增删导致越界返回 None。
    pub(crate) fn framebuffer_id(&self, index: usize) -> Option<u32> {
        self.device
            .state
            .lock()
            .framebuffers
            .iter()
            .nth(index)
            .map(|(&id, _)| id)
    }

    /// @description 查询本 OFD 创建的 legacy framebuffer metadata。
    /// @param id device-wide framebuffer ID。
    /// @return owner 匹配时返回 metadata；未建模 master 权限时 handle 固定为零。
    /// @errors object 不存在或属于其他 OFD 返回 NotFound。
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

    /// @description 读取已经由 GPU completion 确认的 active framebuffer ID。
    /// @return 尚未由 userspace modeset 时返回 None；否则返回 device-wide object ID。
    pub(crate) fn active_framebuffer(&self) -> Option<u32> {
        self.device
            .completion
            .lock()
            .active
            .map(|active| active.framebuffer)
    }

    /// @description 异步提交一个本 OFD framebuffer 为固定 single-scanout backing。
    ///
    /// @param id device-wide framebuffer object ID。
    /// @return CREATE→ATTACH→TRANSFER→SET→FLUSH→UNREF transaction fence。
    /// @errors object/尺寸非法、已有 transaction 或 adapter failure 返回稳定领域错误。
    pub(crate) fn page_flip(
        self: &Arc<Self>,
        id: u32,
        user_data: Option<u64>,
    ) -> Result<DrmWait, DrmError> {
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
        self.submit_scanout(&mut completion, Some(id), event)
    }

    /// @description 同步 modeset 到指定 framebuffer，不忙等 GPU completion。
    /// @param id device-wide framebuffer object ID。
    /// @return hardware transaction 完成且 active state 发布后返回 unit。
    /// @errors page-flip 提交错误、signal interruption 或 wait registration OOM。
    pub(crate) fn set_crtc(&self, id: u32) -> Result<DrmWait, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        self.submit_scanout(&mut completion, Some(id), None)
    }

    /// @description 同步恢复启动期黑屏 backing，并清除 active framebuffer state。
    /// @return fallback transaction 完成后返回 unit。
    /// @errors 已有 transaction、signal interruption、OOM 或 adapter failure。
    pub(crate) fn disable_crtc(&self) -> Result<DrmWait, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        self.submit_scanout(&mut completion, None, None)
    }

    fn submit_scanout(
        &self,
        completion: &mut CompletionState,
        framebuffer_id: Option<u32>,
        event: Option<PendingEvent>,
    ) -> Result<DrmWait, DrmError> {
        if completion.pending.is_some() {
            return Err(DrmError::Busy);
        }
        let (backing, owner) = if let Some(id) = framebuffer_id {
            let state = self.device.state.lock();
            let framebuffer = state.framebuffers.get(&id).ok_or(DrmError::NotFound)?;
            if framebuffer.owner != self.file_identity {
                return Err(DrmError::NotFound);
            }
            let mode = self.device._mode;
            if framebuffer.width != mode.width
                || framebuffer.height != mode.height
                || framebuffer.pitch != mode.pitch
            {
                return Err(DrmError::Invalid);
            }
            (framebuffer.buffer.backing.clone(), Some(self.file_identity))
        } else {
            (self.device.fallback_backing.clone(), None)
        };
        let fence = self
            .device
            .display
            .submit_scanout(backing)
            .map_err(device::display_error)?;
        completion.pending = Some(PendingScanout {
            fence,
            framebuffer: framebuffer_id,
            owner,
            event,
        });
        Ok(DrmWait {
            device: self.device.clone(),
            fence,
        })
    }
}
