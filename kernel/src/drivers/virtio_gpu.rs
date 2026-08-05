use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::memory::{DeviceBacking, FrameAllocationClass, PAGE_SIZE};

use super::{
    CursorCommand, DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate,
    GraphicsDevice, InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING,
    VirglCapsetInfo, VirglCommand, VirtIODevice,
    virtio_queue::{DmaBuffer, VirtQueue, VirtQueueError},
};

mod wire;
use wire::*;
mod boot;
mod command;
use command::{GpuCommand, PendingCommand, PreparedCommand};
mod damage;
mod graphics_command;
use damage::DamageTransition;
mod resource;
use resource::{CursorResourceSet, ResourceSet, RuntimeOperation};
mod sequence;
mod sequence_policy;
use sequence::{SequenceAction, SequenceCompletion};

// Every command consumes one readable and one writable descriptor. Owning half the negotiated
// 64-entry queue lets one complete effect-heavy paint burst publish asynchronously; a smaller
// arbitrary slot count makes the fifth EXECBUFFER sleep behind host completion and blocks input.
const CONTROL_COMMAND_CAPACITY: usize = QUEUE_SIZE as usize / 2;

struct CursorQueue {
    queue: VirtQueue,
    request: DmaBuffer<CURSOR_REQUEST_SIZE>,
    // OWNER: 单 slot 让 request DMA、descriptor head 与 completion sequence 同生共死；
    // 缺失该 latch 时连续 motion 会覆盖 device 尚未读取的光标坐标。
    pending: Option<(u16, u64)>,
    // OWNER: cursorq 忙时只保留尚未发布的最新位置；缺失 coalescing 会迫使 DRM ioctl
    // 等待每个旧坐标完成，鼠标采样率会直接变成 compositor 主循环的阻塞频率。
    latest_move: Option<(u32, u32)>,
    // OWNER: UPDATE 成功发布的可见性必须延续到后续 MOVE wire request。QEMU 10 即使按
    // VirtIO 规范应忽略 MOVE 的其他字段，仍用 resource_id 决定 host pointer 是否显示；
    // 不在 cursorq 唯一保存该状态会让第一条 motion 把 resource_id 清零并隐藏光标。
    visible: bool,
    next_sequence: u64,
}

impl CursorQueue {
    fn submit(&mut self, command: CursorCommand) -> Result<u64, DisplayError> {
        debug_assert!(self.pending.is_none());
        let visible = match command {
            CursorCommand::Update { visible, .. } => Some(visible),
            CursorCommand::Move { .. } => None,
        };
        prepare_cursor(self.request.as_mut_slice(), command)?;
        let request = self
            .request
            .readable(0..CURSOR_REQUEST_SIZE)
            .map_err(|_| DisplayError::Device)?;
        let head = match self.queue.add_dma(&[request]) {
            Ok(head) => head,
            Err(VirtQueueError::NoDescriptors) => return Err(DisplayError::WouldBlock),
            Err(VirtQueueError::InvalidBuffer) => return Err(DisplayError::Device),
        };
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).ok_or(DisplayError::Device)?;
        self.queue.add_to_avail(head);
        self.pending = Some((head, sequence));
        if let Some(visible) = visible {
            self.visible = visible;
        }
        Ok(sequence)
    }
}

struct CommandSlot {
    request: DmaBuffer<CONTROL_REQUEST_SIZE>,
    response: DmaBuffer<CAPSET_RESPONSE_SIZE>,
    pending: Option<PendingCommand>,
}

struct CommandSlots {
    slots: Vec<CommandSlot>,
}

enum CommandPayload {
    None,
    DisplayInfo(DisplayMode),
}

struct CompletedCommand {
    pending: PendingCommand,
    payload: CommandPayload,
}

impl CompletedCommand {
    fn into_parts(self) -> (PendingCommand, CommandPayload) {
        (self.pending, self.payload)
    }
}

impl CommandSlots {
    fn try_new() -> Option<Self> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(CONTROL_COMMAND_CAPACITY).ok()?;
        for _ in 0..CONTROL_COMMAND_CAPACITY {
            slots.push(CommandSlot {
                request: DmaBuffer::try_zeroed().ok()?,
                response: DmaBuffer::try_zeroed().ok()?,
                pending: None,
            });
        }
        Some(Self { slots })
    }

    fn idle_index(&self) -> Option<usize> {
        self.slots.iter().position(|slot| slot.pending.is_none())
    }

    fn has_pending(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending.is_some())
    }

    fn has_non_render_pending(&self) -> bool {
        self.slots.iter().any(|slot| {
            slot.pending.as_ref().is_some_and(|pending| {
                !matches!(
                    pending.stage,
                    sequence_policy::RuntimeStage::Virgl
                        | sequence_policy::RuntimeStage::VirglSetScanout
                        | sequence_policy::RuntimeStage::VirglFlush
                )
            })
        })
    }

    fn idle_pair(&self) -> Option<[usize; 2]> {
        let mut idle = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.pending.is_none().then_some(index));
        Some([idle.next()?, idle.next()?])
    }

    fn request_mut(&mut self, index: usize) -> &mut DmaBuffer<CONTROL_REQUEST_SIZE> {
        assert!(self.slots[index].pending.is_none());
        &mut self.slots[index].request
    }

    fn complete(&mut self, head: u16, length: usize) -> Result<CompletedCommand, DisplayError> {
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| {
                slot.pending
                    .as_ref()
                    .is_some_and(|pending| pending.head == head)
            })
            .ok_or(DisplayError::Device)?;
        let pending = slot.pending.as_ref().ok_or(DisplayError::Device)?;
        if length != pending.response_length()
            || read_u32(slot.response.as_slice(), 0) != Some(pending.stage.expected_response())
            || read_u32(slot.response.as_slice(), 4)
                .is_none_or(|flags| flags & VIRTIO_GPU_FLAG_FENCE == 0)
            || read_u64(slot.response.as_slice(), 8) != Some(pending.command_fence)
        {
            return Err(DisplayError::Device);
        }
        let payload = match pending.stage {
            sequence_policy::RuntimeStage::DisplayInfo => CommandPayload::DisplayInfo(
                VirtIOGpuDevice::parse_display_mode(slot.response.as_slice())
                    .ok_or(DisplayError::Device)?,
            ),
            _ => CommandPayload::None,
        };
        let pending = slot.pending.take().ok_or(DisplayError::Device)?;
        Ok(CompletedCommand { pending, payload })
    }

    fn boot_slot_mut(&mut self) -> &mut CommandSlot {
        assert!(!self.has_pending());
        &mut self.slots[0]
    }

    fn boot_response(&self) -> &DmaBuffer<CAPSET_RESPONSE_SIZE> {
        assert!(!self.has_pending());
        &self.slots[0].response
    }
}

struct ControlQueue {
    queue: VirtQueue,
    // OWNER: invalid controlq completion permanently closes publication until reset.
    failed: bool,
    next_fence: u64,
    // OWNER: 每个 slot 把 request/response/head/fence 保持到对应 used completion；缺少独立
    // DMA storage 时，后提交的 render command 会覆盖仍由 device 读取的前一条 request。
    commands: CommandSlots,
    // OWNER: capset 是 FEATURES_OK 后从 host 固定取得的 immutable VirGL ABI；若每个 DRM
    // OFD 重查，context 可能在 resize/config interrupt 间观察不同 capability generation。
    capset: Option<VirglCapset>,
    // OWNER: resources 唯一拥有两个 fixed resource ID、active slot、backing lifetime 与
    // DIRTYFB synchronization fact；复制 cache 会让 eviction DMA 与 allocator 回收竞态。
    resources: ResourceSet,
    // OWNER: cursor_resource 独占规范要求的固定 64x64 ARGB 2D resource 与 backing。
    // 若复用 VirGL texture，QEMU/UTM 可以完成 cursorq 命令却无法取得标准 2D 像素。
    cursor_resource: CursorResourceSet,
    // OWNER: operation 串联 scanout、damage 或 disable 的唯一多阶段状态；缺失时每个 IRQ
    // stage 无法证明 request、backing 与 operation fence 属于同一事务。
    operation: Option<RuntimeOperation>,
    // OWNER: damage 是 controlq 唯一的固定运行期 clip scratch；只有 operation=Damage 时
    // 内容有效。把它塞进 enum 会让每个非 damage operation 膨胀到 520 bytes，改用 Box
    // 又会让 DIRTYFB 热路径分配并在内存压力下失败。
    damage: DamageTransition,
    // OWNER: config event 可与正在执行的 scanout command 合并到来；该位把一次尚未提交的
    // GET_DISPLAY_INFO 保留到 controlq 空闲，否则清除 device event 后会永久丢失 resize。
    config_change_pending: bool,
    // OWNER: mode 是最新 connector preferred generation；active resource 自带独立 mode，
    // resize 不会偷偷改变当前 CRTC 或触发 allocation/modeset。
    mode: DisplayMode,
}

struct VirglCapset {
    info: VirglCapsetInfo,
    bytes: [u8; MAX_CAPSET_SIZE],
}

/// @description VirtIO-GPU 2D single-scanout adapter。
pub(crate) struct VirtIOGpuDevice {
    device: VirtIODevice,
    context_init: bool,
    // OWNER: adapter 在 device ready 后永久持有 controlq DMA backing；hardirq 只确认
    // MMIO 并发布 deferred bit，controlq completion 只在 user-return/idle safe point 获取
    // 此 ordinary lock。若初始化后释放，device 仍可访问已经归还 allocator 的 pages。
    control: Mutex<ControlQueue>,
    // OWNER: cursorq 是光标移动的唯一 fast path；它与 controlq 分锁，避免 scene render
    // 持锁时把 pointer motion 重新串行到耗时 VirGL command 后面。
    cursor: Mutex<CursorQueue>,
}

impl VirtIOGpuDevice {
    /// @description 初始化 MMIO v2 controlq，查询第一个 enabled scanout 并建立 2D resource。
    ///
    /// @param base_addr DTB VirtIO MMIO 基址。
    /// @return 已绑定单 scanout 的 GPU adapter。
    /// @errors feature、queue、mode、frame allocation 或命令失败返回 `None`。
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 16 {
            return None;
        }
        device.initialize().ok()?;
        let offered = device.device_features().ok()?;
        if offered & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        let graphics_features = VIRTIO_GPU_F_VIRGL | VIRTIO_GPU_F_CONTEXT_INIT;
        let features = VIRTIO_F_VERSION_1 | offered & graphics_features;
        device.set_driver_features(features).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let maximum = device.queue_max_size(CONTROL_QUEUE).ok()?;
        let size = maximum.min(QUEUE_SIZE);
        let queue = VirtQueue::new(size)?;
        device
            .configure_queue(CONTROL_QUEUE, size, queue.addresses())
            .ok()?;
        let control = Mutex::new(ControlQueue {
            queue,
            failed: false,
            next_fence: 1,
            commands: CommandSlots::try_new()?,
            capset: None,
            resources: ResourceSet::empty(),
            cursor_resource: CursorResourceSet::empty(),
            operation: None,
            damage: DamageTransition::try_new()?,
            config_change_pending: false,
            mode: DisplayMode {
                width: 0,
                height: 0,
                pitch: 0,
            },
        });
        let cursor_maximum = device.queue_max_size(CURSOR_QUEUE).ok()?;
        let cursor_size = cursor_maximum.min(QUEUE_SIZE);
        let cursor_queue = VirtQueue::new(cursor_size)?;
        device
            .configure_queue(CURSOR_QUEUE, cursor_size, cursor_queue.addresses())
            .ok()?;
        let cursor = Mutex::new(CursorQueue {
            queue: cursor_queue,
            request: DmaBuffer::try_zeroed().ok()?,
            pending: None,
            latest_move: None,
            visible: false,
            next_sequence: 1,
        });
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        let adapter = Self {
            device,
            context_init: features & VIRTIO_GPU_F_CONTEXT_INIT != 0,
            control,
            cursor,
        };
        if features & VIRTIO_GPU_F_VIRGL != 0 {
            let capset = Self::load_virgl_capset(&adapter.device, &adapter.control)?;
            adapter.control.lock().capset = Some(capset);
        }
        let mode = Self::display_mode(&adapter.device, &adapter.control)?;
        adapter.control.lock().mode = mode;
        let framebuffer_bytes = usize::try_from(mode.pitch)
            .ok()?
            .checked_mul(usize::try_from(mode.height).ok()?)?;
        let framebuffer = Arc::try_new(DeviceBacking::try_allocate(
            framebuffer_bytes.div_ceil(PAGE_SIZE),
            FrameAllocationClass::KernelCritical,
        )?)
        .ok()?;
        Self::initialize_scanout(&adapter.device, &adapter.control, mode, &framebuffer)?;
        adapter.control.lock().resources = ResourceSet::with_boot(framebuffer, mode);

        Arc::try_new(adapter).ok()
    }

    fn submit_command(
        &self,
        control: &mut ControlQueue,
        command: GpuCommand,
        operation_fence: Option<u64>,
    ) -> Result<u64, DisplayError> {
        if control.failed {
            return Err(DisplayError::Device);
        }
        let slot = control
            .commands
            .idle_index()
            .ok_or(DisplayError::WouldBlock)?;
        let prepared = command.prepare(control.commands.request_mut(slot).as_mut_slice())?;
        self.publish_prepared(control, slot, prepared, operation_fence)
    }

    fn publish_prepared(
        &self,
        control: &mut ControlQueue,
        slot_index: usize,
        prepared: PreparedCommand,
        operation_fence: Option<u64>,
    ) -> Result<u64, DisplayError> {
        let command_fence = control.next_fence;
        let next_fence = control
            .next_fence
            .checked_add(1)
            .ok_or(DisplayError::Device)?;
        let response_length =
            if matches!(prepared.stage, sequence_policy::RuntimeStage::DisplayInfo) {
                DISPLAY_INFO_SIZE
            } else {
                CONTROL_HEADER_SIZE
            };
        let slot = &mut control.commands.slots[slot_index];
        write_u32(slot.request.as_mut_slice(), 0, prepared.opcode).ok_or(DisplayError::Device)?;
        write_u32(slot.request.as_mut_slice(), 4, VIRTIO_GPU_FLAG_FENCE)
            .ok_or(DisplayError::Device)?;
        write_u64(slot.request.as_mut_slice(), 8, command_fence).ok_or(DisplayError::Device)?;
        slot.response[..response_length].fill(0);

        let head = {
            let ControlQueue {
                queue, commands, ..
            } = control;
            let slot = &commands.slots[slot_index];
            let request = slot
                .request
                .readable(0..prepared.length)
                .map_err(|_| DisplayError::Device)?;
            let response = slot.response.writable_all();
            match queue.add_dma(&[request, response]) {
                Ok(head) => head,
                Err(VirtQueueError::NoDescriptors) => return Err(DisplayError::WouldBlock),
                Err(VirtQueueError::InvalidBuffer) => return Err(DisplayError::Device),
            }
        };
        // 从 avail publication 开始 command 已不可撤销；先完成所有可失败的本地准备，
        // 再一次性提交 fence、descriptor 与 pending owner。
        control.next_fence = next_fence;
        control.queue.add_to_avail(head);
        let operation_fence = operation_fence.unwrap_or(command_fence);
        let pending = &mut control.commands.slots[slot_index].pending;
        assert!(pending.is_none(), "published VirtIO GPU slot was not idle");
        *pending = Some(PendingCommand {
            head,
            operation_fence,
            command_fence,
            stage: prepared.stage,
        });
        // Doorbell 失败发生在 descriptor 已对 device 可见之后，不能伪装成可重试 EIO：
        // caller 若回滚 backing，device 仍可能 DMA。此时唯一正确语义是 device fail-stop。
        self.device
            .notify_queue(CONTROL_QUEUE)
            .expect("VirtIO GPU doorbell failed after descriptor publication");
        Ok(operation_fence)
    }

    fn add_prepared_descriptor(
        control: &mut ControlQueue,
        slot_index: usize,
        request_length: usize,
    ) -> Result<u16, DisplayError> {
        let ControlQueue {
            queue, commands, ..
        } = control;
        let slot = &commands.slots[slot_index];
        let request = slot
            .request
            .readable(0..request_length)
            .map_err(|_| DisplayError::Device)?;
        let response = slot.response.writable_all();
        match queue.add_dma(&[request, response]) {
            Ok(head) => Ok(head),
            Err(VirtQueueError::NoDescriptors) => Err(DisplayError::WouldBlock),
            Err(VirtQueueError::InvalidBuffer) => Err(DisplayError::Device),
        }
    }

    fn publish_display_info(&self, control: &mut ControlQueue) -> Result<(), DisplayError> {
        self.submit_command(control, GpuCommand::DisplayInfo, Some(0))?;
        Ok(())
    }

    fn publish_damage_batch(
        &self,
        control: &mut ControlQueue,
        operation_fence: Option<u64>,
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<u64, DisplayError> {
        if control.failed {
            return Err(DisplayError::Device);
        }
        let fence = control.damage.publish_next(
            &mut control.queue,
            &mut control.next_fence,
            operation_fence,
            mode,
            resource_id,
        )?;
        // 全部 TRANSFER descriptor 共享一次 doorbell，避免每个 clip 放大成一次 host exit。
        self.device
            .notify_queue(CONTROL_QUEUE)
            .expect("VirtIO GPU batch doorbell failed after descriptor publication");
        Ok(fence)
    }

    fn fail_device(&self) -> DisplayError {
        let first_failure = {
            let mut control = self.control.lock();
            !core::mem::replace(&mut control.failed, true)
        };
        if first_failure {
            let _ = self.device.reset();
        }
        DisplayError::Device
    }

    fn poll_cursor(&self) -> Result<Option<u64>, DisplayError> {
        let mut cursor = self.cursor.lock();
        let used = match cursor.queue.used() {
            Ok(Some(used)) => used,
            Ok(None) => return Ok(None),
            Err(()) => return Err(DisplayError::Device),
        };
        let (head, sequence) = cursor.pending.ok_or(DisplayError::Device)?;
        if used.head() != head || used.length() != 0 {
            return Err(DisplayError::Device);
        }
        cursor
            .queue
            .recycle_used(used)
            .map_err(|_| DisplayError::Device)?;
        cursor.pending = None;
        let notify = if let Some((x, y)) = cursor.latest_move.take() {
            let visible = cursor.visible;
            cursor.submit(CursorCommand::Move { x, y, visible })?;
            true
        } else {
            false
        };
        drop(cursor);
        if notify {
            self.device
                .notify_queue(CURSOR_QUEUE)
                .expect("VirtIO GPU cursor doorbell failed after coalesced move publication");
        }
        Ok(Some(sequence))
    }

    fn apply_sequence_action(
        &self,
        control: &mut ControlQueue,
        action: SequenceAction,
    ) -> Result<Option<SequenceCompletion>, DisplayError> {
        match action {
            SequenceAction::Command {
                command,
                operation_fence,
            } => {
                self.submit_command(control, command, Some(operation_fence))?;
                Ok(None)
            }
            SequenceAction::DamageBatch {
                operation_fence,
                mode,
                resource_id,
            } => {
                self.publish_damage_batch(control, Some(operation_fence), mode, resource_id)?;
                Ok(None)
            }
            SequenceAction::Finished(completion) => Ok(Some(completion)),
        }
    }

    /// @description 构造持有 GPU owner 的 IRQ handler。
    ///
    /// @return 只确认 control/config interrupt 的 handler。
    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIOGpuIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO GPU IRQ handler allocation failed")
    }
}

struct VirtIOGpuIrqHandler {
    device: Arc<VirtIOGpuDevice>,
}

impl InterruptHandler for VirtIOGpuIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let status = self
            .device
            .device
            .interrupt_status()
            .map_err(|_| InterruptError::DeviceFailure)?;
        self.device
            .device
            .interrupt_ack(status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG))
            .map_err(|_| InterruptError::DeviceFailure)?;
        if status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG) != 0 {
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::Display);
        }
        Ok(())
    }
}

impl DisplayDevice for VirtIOGpuDevice {
    fn mode(&self) -> DisplayMode {
        self.control.lock().mode
    }

    fn submit_scanout(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        self.submit_resident_scanout(identity, mode, backing)
    }

    fn submit_damage(
        &self,
        identity: u64,
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
        rectangles: &[DisplayRect],
    ) -> Result<u64, DisplayError> {
        self.submit_resident_damage(identity, mode, backing, rectangles)
    }

    fn release_buffer(&self, identity: u64) -> Result<Option<u64>, DisplayError> {
        self.release_resident(identity)
    }

    fn disable_scanout(&self) -> Result<u64, DisplayError> {
        self.disable_resident()
    }

    fn poll_update(&self) -> Result<Option<DisplayUpdate>, DisplayError> {
        match self.poll_cursor() {
            Ok(Some(sequence)) => {
                // 一个合并 IRQ 还可能同时携带 controlq completion；再次调度一次，避免
                // cursor fast path 吞掉 scene waiter 的唯一 completion edge。
                crate::cpu::raise_deferred(crate::cpu::DeferredWork::Display);
                return Ok(Some(DisplayUpdate::CursorCompleted(sequence)));
            }
            Ok(None) => {}
            Err(_) => return Err(self.fail_device()),
        }
        let mut control = self.control.lock();
        if control.failed {
            return Err(DisplayError::Device);
        }
        let events = match self.device.read_config_u32(VIRTIO_GPU_EVENTS_READ) {
            Ok(events) => events,
            Err(_) => {
                drop(control);
                return Err(self.fail_device());
            }
        };
        if events != 0 {
            if self
                .device
                .write_config_u32(VIRTIO_GPU_EVENTS_CLEAR, events)
                .is_err()
            {
                drop(control);
                return Err(self.fail_device());
            }
            control.config_change_pending |= events & VIRTIO_GPU_EVENT_DISPLAY != 0;
        }

        let action = if control.damage.batch_active() {
            loop {
                let used = match control.queue.used() {
                    Ok(Some(used)) => used,
                    Ok(None) => return Ok(None),
                    Err(()) => {
                        drop(control);
                        return Err(self.fail_device());
                    }
                };
                let complete = match control.damage.complete(used.head(), used.length() as usize) {
                    Ok(complete) => complete,
                    Err(_) => {
                        drop(control);
                        return Err(self.fail_device());
                    }
                };
                if control.queue.recycle_used(used).is_err() {
                    drop(control);
                    return Err(self.fail_device());
                }
                if complete {
                    break;
                }
            }
            match sequence::finish_damage_batch(&mut control) {
                Ok(action) => action,
                Err(_) => {
                    drop(control);
                    return Err(self.fail_device());
                }
            }
        } else {
            let used = match control.queue.used() {
                Ok(Some(used)) => used,
                Ok(None) => {
                    if !control.commands.has_pending() && control.config_change_pending {
                        control.config_change_pending = false;
                        if self.publish_display_info(&mut control).is_err() {
                            drop(control);
                            return Err(self.fail_device());
                        }
                    }
                    return Ok(None);
                }
                Err(()) => {
                    drop(control);
                    return Err(self.fail_device());
                }
            };
            let completed = match control
                .commands
                .complete(used.head(), used.length() as usize)
            {
                Ok(completed) => completed,
                Err(_) => {
                    drop(control);
                    return Err(self.fail_device());
                }
            };
            let action = match sequence::complete(&mut control, completed) {
                Ok(action) => action,
                Err(_) => {
                    drop(control);
                    return Err(self.fail_device());
                }
            };
            if control.queue.recycle_used(used).is_err() {
                drop(control);
                return Err(self.fail_device());
            }
            action
        };

        let completion = match self.apply_sequence_action(&mut control, action) {
            Ok(completion) => completion,
            Err(_) => {
                drop(control);
                return Err(self.fail_device());
            }
        };
        if !control.commands.has_pending() && control.config_change_pending {
            control.config_change_pending = false;
            if self.publish_display_info(&mut control).is_err() {
                drop(control);
                return Err(self.fail_device());
            }
        }
        if control.queue.has_used() {
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::Display);
        }
        if let Some(completion) = completion {
            drop(control);
            completion.retirement.release_after_unlock();
            return Ok(completion.update);
        }
        Ok(None)
    }
}

impl GraphicsDevice for VirtIOGpuDevice {
    fn submit_cursor_upload(
        &self,
        identity: u64,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        self.submit_cursor_resource_upload(identity, backing)
    }

    fn submit_cursor(&self, command: CursorCommand) -> Result<u64, DisplayError> {
        let mut cursor = self.cursor.lock();
        if cursor.pending.is_some() || cursor.latest_move.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let sequence = cursor.submit(command)?;
        drop(cursor);
        self.device
            .notify_queue(CURSOR_QUEUE)
            .expect("VirtIO GPU cursor doorbell failed after descriptor publication");
        Ok(sequence)
    }

    fn move_cursor(&self, x: u32, y: u32) -> Result<(), DisplayError> {
        let mut cursor = self.cursor.lock();
        if cursor.pending.is_some() {
            cursor.latest_move = Some((x, y));
            return Ok(());
        }
        let visible = cursor.visible;
        cursor.submit(CursorCommand::Move { x, y, visible })?;
        drop(cursor);
        self.device
            .notify_queue(CURSOR_QUEUE)
            .expect("VirtIO GPU cursor doorbell failed after move publication");
        Ok(())
    }

    fn virgl_capset_info(&self) -> Option<VirglCapsetInfo> {
        self.control
            .lock()
            .capset
            .as_ref()
            .map(|capset| capset.info)
    }

    fn supports_virgl_context_init(&self) -> bool {
        self.context_init
    }

    fn copy_virgl_capset(&self, output: &mut [u8]) -> Result<usize, DisplayError> {
        let control = self.control.lock();
        let capset = control.capset.as_ref().ok_or(DisplayError::Device)?;
        let destination = output
            .get_mut(..capset.info.size)
            .ok_or(DisplayError::Device)?;
        destination.copy_from_slice(&capset.bytes[..capset.info.size]);
        Ok(capset.info.size)
    }

    fn submit_virgl(&self, command: VirglCommand<'_>) -> Result<u64, DisplayError> {
        let mut control = self.control.lock();
        if control.capset.is_none()
            || control.operation.is_some()
            || control.commands.has_non_render_pending()
            || control.damage.batch_active()
        {
            return Err(if control.capset.is_none() {
                DisplayError::Device
            } else {
                DisplayError::WouldBlock
            });
        }
        let slot = control
            .commands
            .idle_index()
            .ok_or(DisplayError::WouldBlock)?;
        let prepared =
            graphics_command::prepare(command, control.commands.request_mut(slot).as_mut_slice())?;
        self.publish_prepared(&mut control, slot, prepared, None)
    }

    fn submit_virgl_scanout(
        &self,
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<u64, DisplayError> {
        let mut control = self.control.lock();
        if control.capset.is_none()
            || control.operation.is_some()
            || control.commands.has_non_render_pending()
            || control.damage.batch_active()
        {
            return Err(if control.capset.is_none() {
                DisplayError::Device
            } else {
                DisplayError::WouldBlock
            });
        }
        if mode != control.mode || resource_id == 0 {
            return Err(DisplayError::InvalidRectangle);
        }
        let [set_slot, flush_slot] = control
            .commands
            .idle_pair()
            .ok_or(DisplayError::WouldBlock)?;

        // 1. 在触碰 descriptor free-list 前完整编码并验证 SET_SCANOUT→RESOURCE_FLUSH。
        // 缺少后者时 QEMU 只替换 scanout texture，不会向 SPICE/GL display 发 dpy_gl_update。
        let mut set_scanout = graphics_command::prepare(
            VirglCommand::SetScanout { mode, resource_id },
            control.commands.request_mut(set_slot).as_mut_slice(),
        )?;
        set_scanout.stage = sequence_policy::RuntimeStage::VirglSetScanout;
        let mut flush = graphics_command::prepare(
            VirglCommand::Flush {
                resource_id,
                rectangle: DisplayRect {
                    x: 0,
                    y: 0,
                    width: mode.width,
                    height: mode.height,
                },
            },
            control.commands.request_mut(flush_slot).as_mut_slice(),
        )?;
        flush.stage = sequence_policy::RuntimeStage::VirglFlush;
        set_scanout
            .stage
            .validate_successor(flush.stage)
            .map_err(|_| DisplayError::Device)?;

        let operation_fence = control.next_fence;
        let flush_fence = operation_fence.checked_add(1).ok_or(DisplayError::Device)?;
        let next_fence = flush_fence.checked_add(1).ok_or(DisplayError::Device)?;
        for (slot_index, prepared, command_fence) in [
            (set_slot, &set_scanout, operation_fence),
            (flush_slot, &flush, flush_fence),
        ] {
            let slot = &mut control.commands.slots[slot_index];
            write_u32(slot.request.as_mut_slice(), 0, prepared.opcode)
                .ok_or(DisplayError::Device)?;
            write_u32(slot.request.as_mut_slice(), 4, VIRTIO_GPU_FLAG_FENCE)
                .ok_or(DisplayError::Device)?;
            write_u64(slot.request.as_mut_slice(), 8, command_fence).ok_or(DisplayError::Device)?;
            slot.response[..CONTROL_HEADER_SIZE].fill(0);
        }

        // 2. 两条 chain 都成功占有 descriptor 后才建立 pending owner。第二条构造失败时，
        // 第一条仍未进入 avail ring，可沿唯一 rollback seam 完整归还。
        let set_head = Self::add_prepared_descriptor(&mut control, set_slot, set_scanout.length)?;
        let flush_head = match Self::add_prepared_descriptor(&mut control, flush_slot, flush.length)
        {
            Ok(head) => head,
            Err(error) => {
                if control.queue.retire_unpublished(set_head).is_err() {
                    return Err(DisplayError::Device);
                }
                return Err(error);
            }
        };
        control.next_fence = next_fence;
        control.commands.slots[set_slot].pending = Some(PendingCommand {
            head: set_head,
            operation_fence,
            command_fence: operation_fence,
            stage: set_scanout.stage,
        });
        control.commands.slots[flush_slot].pending = Some(PendingCommand {
            head: flush_head,
            operation_fence,
            command_fence: flush_fence,
            stage: flush.stage,
        });

        // 3. 一次 Release publication 与一次 doorbell 同时暴露完整 transaction；device 保持
        // controlq 顺序，而客体不再为两条 command 各付一次 completion→submit 往返。
        control.queue.add_batch_to_avail(&[set_head, flush_head]);
        self.device
            .notify_queue(CONTROL_QUEUE)
            .expect("VirtIO GPU doorbell failed after scanout batch publication");
        Ok(operation_fence)
    }
}

impl Drop for VirtIOGpuDevice {
    fn drop(&mut self) {
        // Reset revokes every published descriptor before controlq and cached DMA mappings drop.
        // Without this ordering, failed initialization or final Arc release can free live pages.
        let _ = self.device.reset();
    }
}
