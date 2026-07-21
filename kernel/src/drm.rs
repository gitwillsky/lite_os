use alloc::sync::{Arc, Weak};
use spin::Mutex;

pub(crate) use crate::drivers::DisplayRect;
use crate::{
    drivers::{DisplayDevice, DisplayMode},
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeEnd},
    memory::{DeviceBacking, DeviceMappingSource, FrameAllocationClass, PAGE_SIZE},
};

const DUMB_OFFSET_SHIFT: u32 = 32;

mod event;
pub(crate) use event::DrmEvent;
use event::{EVENT_QUEUE_CAPACITY, EventQueue};
pub(crate) mod device;
mod master;
mod mode;
mod publication;
mod publication_order;
pub(crate) use publication::{PreparedDumbBuffer, PreparedFramebuffer};
use publication_order::IdAllocator;

struct CompletionState {
    // OWNER: pending 同时绑定 adapter fence 与 scanout/damage/disable 领域结果；若拆分，
    // completion 与并发 RMFB/close 会把 active state 发布到错误 object。
    pending: Option<PendingDisplay>,
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
    buffer_identities: IdAllocator<u64>,
    next_file_identity: u64,
    // OWNER: framebuffer allocator 与 device-wide object map 同锁；rollback storage 在 reserve
    // 时预留，copyout failure 可按任意并发顺序无分配回收未发布 ID。
    framebuffer_ids: IdAllocator<u32>,
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
    buffer: Arc<DumbBuffer>,
}

struct DrmFileState {
    // OWNER: handle allocator 与同 OFD map 共用 transaction lock；若独立递增，两个并发
    // CREATE_DUMB 可预留同一 handle，后提交者会覆盖前一个 object access。
    handle_ids: IdAllocator<u32>,
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
    // OWNER: 每个 OFD 的 handle namespace 与 handle allocator 在同一锁内发布；若放到 device
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
    /// scanout disable 或 inactive RESOURCE_UNREF 尚未完成，caller 必须等待后重试删除。
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
    /// @description 准备 file-private XRGB8888 linear dumb buffer，不提前发布 handle。
    ///
    /// @param width 非零 pixel width。
    /// @param height 非零 pixel height。
    /// @param bpp 仅支持标准 XRGB8888 color mode 32。
    /// @param flags Linux UAPI 要求为零。
    /// @return 已预留全部 fallible storage 的 publication transaction。
    /// @errors 参数/溢出返回 Invalid；frame/control/node OOM 返回 OutOfMemory；identity/handle
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

    /// @description 为 file-private dumb handle 返回后续 mmap 使用的 fake byte offset。
    /// @param handle 当前 OFD namespace 内的 GEM handle。
    /// @return handle 仍 live 时返回 page-aligned、非零且同 OFD 稳定的 offset。
    /// @errors handle 不存在返回 NotFound。
    pub(crate) fn map_dumb(&self, handle: u32) -> Result<u64, DrmError> {
        // 临时跟踪：lookup 失败时打印当前 namespace（排查 SET_BUFFER adopt 后移除）。
        let state = self.state.lock();
        if handle == 0 || !state.buffers.contains_key(&handle) {
            let mut keys = alloc::vec::Vec::new();
            for (key, _) in state.buffers.iter() {
                keys.push(*key);
            }
            crate::warn!(
                "[DRM] map_dumb miss: handle={} file_identity={} keys={:?}",
                handle,
                self.file_identity,
                keys
            );
            return Err(DrmError::NotFound);
        }
        Ok(u64::from(handle) << DUMB_OFFSET_SHIFT)
    }

    /// @description 删除 file-private GEM handle；已有 VMA 继续独立保活 backing。
    /// @param handle 当前 OFD namespace 内的 handle。
    /// @return 删除成功返回 unit。
    /// @errors handle 不存在返回 NotFound。
    pub(crate) fn destroy_dumb(&self, handle: u32) -> Result<(), DrmError> {
        // 临时跟踪：记录 handle 销毁（排查 SET_BUFFER adopt 后移除）。
        crate::warn!(
            "[DRM] destroy_dumb: handle={} file_identity={}",
            handle,
            self.file_identity
        );
        let removed = self.state.lock().buffers.remove(&handle);
        let buffer = removed.ok_or(DrmError::NotFound)?;
        // DeviceBacking 的最后一个 Arc 会逐 extent 进入 buddy merge；必须在 GEM
        // namespace lock 外析构，否则回收会把 allocator lock 嵌套进 OFD transaction。
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

    /// @description 准备 device-wide legacy framebuffer object，不提前发布 ID。
    ///
    /// @param handle 当前 OFD 的 dumb handle。
    /// @param width framebuffer pixel width。
    /// @param height framebuffer pixel height。
    /// @param pitch linear scanline bytes，必须与 dumb allocation 一致。
    /// @return 已预留全部 fallible storage 的 publication transaction。
    /// @errors handle/尺寸非法返回对应错误；ID/node 耗尽返回 NoSpace/OutOfMemory。
    pub(crate) fn prepare_framebuffer(
        &self,
        handle: u32,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> Result<PreparedFramebuffer<'_>, DrmError> {
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
        let id = publication::FramebufferIdReservation::reserve(self)?;
        let entry = FallibleMap::try_prepare(
            id.id,
            Framebuffer {
                owner: self.file_identity,
                width,
                height,
                pitch,
                buffer,
            },
        )
        .map_err(|_| DrmError::OutOfMemory)?;
        Ok(PreparedFramebuffer::new(id, entry))
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
        let mode = completion
            .active
            .map(|active| active.mode)
            .ok_or(DrmError::Invalid)?;
        self.submit_scanout(&mut completion, mode, id, event)
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
        let mode = self.device.state.lock().mode;
        self.submit_scanout(&mut completion, mode, id, None)
    }

    /// @description 同步把任一本 OFD framebuffer 的 dirty rectangles 传输到 resident resource。
    /// @param id 属于本 OFD 的 framebuffer object ID；允许在 page flip 前同步 inactive buffer。
    /// @param rectangles 0..=32 个半开 scanout rectangle；零个表示 full framebuffer。
    /// @return Linux 语义下零 clips 扩展为 full framebuffer；始终返回 TRANSFER+FLUSH wait token。
    /// @errors framebuffer 非本 OFD、已有 operation 或 rectangle/device failure。
    pub(crate) fn dirty_framebuffer(
        &self,
        id: u32,
        rectangles: &[DisplayRect],
    ) -> Result<DrmWait, DrmError> {
        let mut completion = self.device.completion.lock();
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
        self.submit_damage(
            &mut completion,
            id,
            if rectangles.is_empty() {
                &full
            } else {
                rectangles
            },
        )
    }

    /// @description 同步以 resource_id=0 禁用 scanout，并清除 active framebuffer state。
    /// @return hardware 解绑 backing 后返回 unit。
    /// @errors 已有 transaction、signal interruption、OOM 或 adapter failure。
    pub(crate) fn disable_crtc(&self) -> Result<DrmWait, DrmError> {
        if !self.is_master() {
            return Err(DrmError::Permission);
        }
        let mut completion = self.device.completion.lock();
        self.submit_disable(&mut completion)
    }
}
