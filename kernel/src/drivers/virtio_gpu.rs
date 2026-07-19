use alloc::sync::Arc;
use spin::Mutex;

use crate::memory::{DeviceBacking, FrameAllocationClass, PAGE_SIZE};

use super::{
    DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate, InterruptError,
    InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK,
    VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
    virtio_queue::{DmaBuffer, VirtQueue},
};

mod wire;
use wire::*;
mod boot;
mod command;
use command::{GpuCommand, PendingCommand, PreparedCommand};
mod damage;
use damage::DamageTransition;
mod resource;
use resource::{ResourceSet, RuntimeOperation};
mod sequence;
mod sequence_policy;
use sequence::{SequenceAction, SequenceCompletion};

struct ControlQueue {
    queue: VirtQueue,
    // OWNER: invalid controlq completion permanently closes publication until reset.
    failed: bool,
    next_fence: u64,
    // 最大 attach request 固定容纳 DeviceBacking 的完整 extent contract；运行期所有
    // command 复用这块 DMA-stable storage，不按 damage/resize 分配临时 request。
    request: DmaBuffer<ATTACH_REQUEST_SIZE>,
    response: DmaBuffer<DISPLAY_INFO_SIZE>,
    // OWNER: pending 是 descriptor head、command fence 与 stage 的唯一对应关系；若按
    // command type 分散记录，乱序或 stale completion 会推进错误 transaction。
    pending: Option<PendingCommand>,
    // OWNER: resources 唯一拥有两个 fixed resource ID、active slot、backing lifetime 与
    // DIRTYFB synchronization fact；复制 cache 会让 eviction DMA 与 allocator 回收竞态。
    resources: ResourceSet,
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

/// @description VirtIO-GPU 2D single-scanout adapter。
pub(crate) struct VirtIOGpuDevice {
    device: VirtIODevice,
    // OWNER: adapter 在 device ready 后永久持有 controlq DMA backing；hardirq 只确认
    // MMIO 并发布 deferred bit，controlq completion 只在 user-return/idle safe point 获取
    // 此 ordinary lock。若初始化后释放，device 仍可访问已经归还 allocator 的 pages。
    control: Mutex<ControlQueue>,
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
        if device.device_features().ok()? & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        device.set_driver_features(VIRTIO_F_VERSION_1).ok()?;
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
            request: DmaBuffer::try_zeroed().ok()?,
            response: DmaBuffer::try_zeroed().ok()?,
            pending: None,
            resources: ResourceSet::empty(),
            operation: None,
            damage: DamageTransition::try_new()?,
            config_change_pending: false,
            mode: DisplayMode {
                width: 0,
                height: 0,
                pitch: 0,
            },
        });
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        let adapter = Self { device, control };
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
        let prepared = command.prepare(control.request.as_mut_slice())?;
        self.publish_prepared(control, prepared, operation_fence)
    }

    fn publish_prepared(
        &self,
        control: &mut ControlQueue,
        prepared: PreparedCommand,
        operation_fence: Option<u64>,
    ) -> Result<u64, DisplayError> {
        let command_fence = control.next_fence;
        let next_fence = control
            .next_fence
            .checked_add(1)
            .ok_or(DisplayError::Device)?;
        write_u32(control.request.as_mut_slice(), 0, prepared.opcode)
            .ok_or(DisplayError::Device)?;
        write_u32(control.request.as_mut_slice(), 4, VIRTIO_GPU_FLAG_FENCE)
            .ok_or(DisplayError::Device)?;
        write_u64(control.request.as_mut_slice(), 8, command_fence).ok_or(DisplayError::Device)?;
        control.response.fill(0);

        let head = {
            let ControlQueue {
                queue,
                request,
                response,
                ..
            } = control;
            let request = request
                .readable(0..prepared.length)
                .map_err(|_| DisplayError::Device)?;
            let response = response.writable_all();
            queue
                .add_dma(&[request, response])
                .map_err(|_| DisplayError::Device)?
        };
        // 从 avail publication 开始 command 已不可撤销；先完成所有可失败的本地准备，
        // 再一次性提交 fence、descriptor 与 pending owner。
        control.next_fence = next_fence;
        control.queue.add_to_avail(head);
        let operation_fence = operation_fence.unwrap_or(command_fence);
        control.pending = Some(PendingCommand {
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
                    if control.pending.is_none() && control.config_change_pending {
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
            let expected_length = match control.pending.as_ref() {
                Some(pending) => pending.response_length(),
                None => {
                    drop(control);
                    return Err(self.fail_device());
                }
            };
            if used.length() as usize != expected_length {
                drop(control);
                return Err(self.fail_device());
            }
            let action = match sequence::complete(&mut control, used.head()) {
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
        if control.pending.is_none() && control.config_change_pending {
            control.config_change_pending = false;
            if self.publish_display_info(&mut control).is_err() {
                drop(control);
                return Err(self.fail_device());
            }
        }
        if let Some(completion) = completion {
            drop(control);
            completion.retirement.release_after_unlock();
            return Ok(completion.update);
        }
        Ok(None)
    }
}

impl Drop for VirtIOGpuDevice {
    fn drop(&mut self) {
        // Reset revokes every published descriptor before controlq and cached DMA mappings drop.
        // Without this ordering, failed initialization or final Arc release can free live pages.
        let _ = self.device.reset();
    }
}
