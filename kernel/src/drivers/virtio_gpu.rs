use alloc::sync::Arc;
use spin::Mutex;

use crate::memory::{DeviceBacking, FrameAllocationClass, PAGE_SIZE};

use super::{
    DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate, InterruptError,
    InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK,
    VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
    virtio_queue::VirtQueue,
};

mod wire;
use wire::*;
mod boot;

const CONTROL_QUEUE: u32 = 0;
const QUEUE_SIZE: u16 = 64;
const MAX_DAMAGE_RECTS: usize = 32;
const ATTACH_REQUEST_SIZE: usize = 32 + DeviceBacking::MAX_EXTENTS * 16;

#[derive(Clone, Copy)]
enum RuntimeStage {
    DisplayInfo,
    Create,
    Attach,
    TransferScanout,
    SetScanout,
    FlushScanout,
    UnrefReplaced,
    TransferDamage,
    FlushDamage,
    DisableScanout,
    UnrefDisabled,
}

struct ScanoutResource {
    id: u32,
    // OWNER: resource backing 必须活到 RESOURCE_UNREF completion；只记录 PPN 会让
    // userspace close/RMFB 后 allocator 重用 device 仍可 DMA 的 extent。
    backing: Arc<DeviceBacking>,
    mode: DisplayMode,
}

struct ScanoutTransition {
    // OWNER: next resource 从 CREATE publication 到最终成为 active 始终由 controlq
    // transaction 保活；缺失该字段会在多阶段 IRQ 间隙释放 backing。
    next: ScanoutResource,
}

struct DamageTransition {
    // OWNER: DIRTYFB 的 userspace clip array 只能在 ioctl frame 内存在；固定副本让后续
    // completion 不访问 userspace，也保证 operation 内绝不分配。
    rectangles: [DisplayRect; MAX_DAMAGE_RECTS],
    count: u8,
    index: u8,
}

enum RuntimeOperation {
    Scanout(ScanoutTransition),
    Damage(DamageTransition),
    Disable,
}

struct PendingCommand {
    head: u16,
    operation_fence: u64,
    command_fence: u64,
    stage: RuntimeStage,
}

struct ControlQueue {
    queue: VirtQueue,
    next_fence: u64,
    // 最大 attach request 固定容纳 DeviceBacking 的完整 extent contract；运行期所有
    // command 复用这块 DMA-stable storage，不按 damage/resize 分配临时 request。
    request: [u8; ATTACH_REQUEST_SIZE],
    response: [u8; DISPLAY_INFO_SIZE],
    // OWNER: pending 是 descriptor head、command fence 与 stage 的唯一对应关系；若按
    // command type 分散记录，乱序或 stale completion 会推进错误 transaction。
    pending: Option<PendingCommand>,
    // OWNER: active resource 保活当前 hardware scanout；只在旧 resource UNREF completion
    // 后替换，避免 device 与 allocator 并发访问同一物理 extent。
    active: Option<ScanoutResource>,
    // OWNER: operation 串联 scanout、damage 或 disable 的唯一多阶段状态；缺失时每个 IRQ
    // stage 无法证明 request、backing 与 operation fence 属于同一事务。
    operation: Option<RuntimeOperation>,
    // 两个 resource ID 只有在 UNREF completion 后才放回固定 free slots。用单 Option 会
    // 在 disable 后丢失第二个 free ID，导致下一次真实 framebuffer switch 无法进行。
    free_resource_ids: [Option<u32>; 2],
    // OWNER: config event 可与正在执行的 scanout command 合并到来；该位把一次尚未提交的
    // GET_DISPLAY_INFO 保留到 controlq 空闲，否则清除 device event 后会永久丢失 resize。
    config_change_pending: bool,
    // OWNER: mode 是最新 connector preferred generation；active resource 自带独立 mode，
    // resize 不会偷偷改变当前 CRTC 或触发 allocation/modeset。
    mode: DisplayMode,
}

impl ControlQueue {
    fn take_resource_id(&mut self) -> Option<u32> {
        self.free_resource_ids.iter_mut().find_map(Option::take)
    }

    fn release_resource_id(&mut self, id: u32) -> Result<(), DisplayError> {
        let slot = self
            .free_resource_ids
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(DisplayError::Device)?;
        *slot = Some(id);
        Ok(())
    }
}

/// @description VirtIO-GPU 2D single-scanout adapter。
pub(super) struct VirtIOGpuDevice {
    device: VirtIODevice,
    // OWNER: adapter 在 device ready 后永久持有 controlq DMA backing；若初始化后释放，
    // device 仍可访问已经归还 allocator 的 descriptor pages。
    control: Mutex<ControlQueue>,
}

impl VirtIOGpuDevice {
    /// @description 初始化 MMIO v2 controlq，查询第一个 enabled scanout 并建立 2D resource。
    ///
    /// @param base_addr DTB VirtIO MMIO 基址。
    /// @return 已绑定单 scanout 的 GPU adapter。
    /// @errors feature、queue、mode、frame allocation 或命令失败返回 `None`。
    pub(super) fn new(base_addr: usize) -> Option<Arc<Self>> {
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
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;

        let control = Mutex::new(ControlQueue {
            queue,
            next_fence: 1,
            request: [0; ATTACH_REQUEST_SIZE],
            response: [0; DISPLAY_INFO_SIZE],
            pending: None,
            active: None,
            operation: None,
            free_resource_ids: [None, None],
            config_change_pending: false,
            mode: DisplayMode {
                width: 0,
                height: 0,
                pitch: 0,
            },
        });
        let mode = Self::display_mode(&device, &control)?;
        control.lock().mode = mode;
        let framebuffer_bytes = usize::try_from(mode.pitch)
            .ok()?
            .checked_mul(usize::try_from(mode.height).ok()?)?;
        let framebuffer = Arc::try_new(DeviceBacking::try_allocate(
            framebuffer_bytes.div_ceil(PAGE_SIZE),
            FrameAllocationClass::KernelCritical,
        )?)
        .ok()?;
        Self::initialize_scanout(&device, &control, mode, &framebuffer)?;

        {
            let mut control = control.lock();
            control.active = Some(ScanoutResource {
                id: BOOT_RESOURCE_ID,
                backing: framebuffer,
                mode,
            });
            control.free_resource_ids[0] = Some(ALTERNATE_RESOURCE_ID);
        }

        Arc::try_new(Self { device, control }).ok()
    }

    fn publish_runtime(
        &self,
        control: &mut ControlQueue,
        command: u32,
        request_length: usize,
        operation_fence: Option<u64>,
        stage: RuntimeStage,
    ) -> Result<u64, DisplayError> {
        let command_fence = control.next_fence;
        let next_fence = control
            .next_fence
            .checked_add(1)
            .ok_or(DisplayError::Device)?;
        write_u32(&mut control.request, 0, command).ok_or(DisplayError::Device)?;
        write_u32(&mut control.request, 4, VIRTIO_GPU_FLAG_FENCE).ok_or(DisplayError::Device)?;
        write_u64(&mut control.request, 8, command_fence).ok_or(DisplayError::Device)?;
        control.response.fill(0);

        let ControlQueue {
            queue,
            request,
            response,
            ..
        } = control;
        let inputs = [&request[..request_length]];
        let mut outputs = [&mut response[..]];
        let head = queue
            .add_buffer(&inputs, &mut outputs)
            .ok_or(DisplayError::Device)?;
        // 从 avail publication 开始 command 已不可撤销；先完成所有可失败的本地准备，
        // 再一次性提交 fence、descriptor 与 pending owner。
        control.next_fence = next_fence;
        queue.add_to_avail(head);
        let operation_fence = operation_fence.unwrap_or(command_fence);
        control.pending = Some(PendingCommand {
            head,
            operation_fence,
            command_fence,
            stage,
        });
        // Doorbell 失败发生在 descriptor 已对 device 可见之后，不能伪装成可重试 EIO：
        // caller 若回滚 backing，device 仍可能 DMA。此时唯一正确语义是 device fail-stop。
        self.device
            .notify_queue(CONTROL_QUEUE)
            .expect("VirtIO GPU doorbell failed after descriptor publication");
        Ok(operation_fence)
    }

    fn publish_display_info(&self, control: &mut ControlQueue) -> Result<(), DisplayError> {
        control.request.fill(0);
        self.publish_runtime(
            control,
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            CONTROL_HEADER_SIZE,
            Some(0),
            RuntimeStage::DisplayInfo,
        )?;
        Ok(())
    }

    /// @description 构造持有 GPU owner 的 IRQ handler。
    ///
    /// @return 只确认 control/config interrupt 的 handler。
    pub(super) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
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
            crate::arch::hart::raise_display_softirq();
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
        mode: DisplayMode,
        backing: Arc<DeviceBacking>,
    ) -> Result<u64, DisplayError> {
        let bytes = usize::try_from(mode.pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(mode.height as usize))
            .ok_or(DisplayError::InvalidRectangle)?;
        if backing
            .pages()
            .checked_mul(PAGE_SIZE)
            .is_none_or(|capacity| capacity < bytes)
            || u32::try_from(bytes).is_err()
        {
            return Err(DisplayError::InvalidRectangle);
        }
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        if control.mode != mode {
            return Err(DisplayError::InvalidRectangle);
        }
        let resource_id = control.take_resource_id().ok_or(DisplayError::Device)?;
        prepare_create(&mut control.request, mode, resource_id)?;
        control.operation = Some(RuntimeOperation::Scanout(ScanoutTransition {
            next: ScanoutResource {
                id: resource_id,
                backing,
                mode,
            },
        }));
        let result = self.publish_runtime(
            &mut control,
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
            40,
            None,
            RuntimeStage::Create,
        );
        if result.is_err() {
            let operation = control.operation.take();
            control
                .release_resource_id(resource_id)
                .expect("unpublished scanout resource ID lost its free slot");
            drop(control);
            drop(operation);
        }
        result
    }

    fn submit_damage(&self, rectangles: &[DisplayRect]) -> Result<u64, DisplayError> {
        if rectangles.is_empty() || rectangles.len() > MAX_DAMAGE_RECTS {
            return Err(DisplayError::InvalidRectangle);
        }
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let active = control.active.as_ref().ok_or(DisplayError::Device)?;
        let mode = active.mode;
        let resource_id = active.id;
        let mut copied = [DisplayRect::default(); MAX_DAMAGE_RECTS];
        for (destination, rectangle) in copied.iter_mut().zip(rectangles.iter().copied()) {
            if rectangle.width == 0
                || rectangle.height == 0
                || rectangle
                    .x
                    .checked_add(rectangle.width)
                    .is_none_or(|right| right > mode.width)
                || rectangle
                    .y
                    .checked_add(rectangle.height)
                    .is_none_or(|bottom| bottom > mode.height)
            {
                return Err(DisplayError::InvalidRectangle);
            }
            *destination = rectangle;
        }
        prepare_transfer(&mut control.request, mode, copied[0], resource_id)?;
        control.operation = Some(RuntimeOperation::Damage(DamageTransition {
            rectangles: copied,
            count: rectangles.len() as u8,
            index: 0,
        }));
        let result = self.publish_runtime(
            &mut control,
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
            56,
            None,
            RuntimeStage::TransferDamage,
        );
        if result.is_err() {
            control.operation = None;
        }
        result
    }

    fn disable_scanout(&self) -> Result<u64, DisplayError> {
        let mut control = self.control.lock();
        if control.pending.is_some() || control.operation.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let mode = control
            .active
            .as_ref()
            .map(|active| active.mode)
            .ok_or(DisplayError::Device)?;
        prepare_set_scanout(&mut control.request, mode, 0)?;
        control.operation = Some(RuntimeOperation::Disable);
        let result = self.publish_runtime(
            &mut control,
            VIRTIO_GPU_CMD_SET_SCANOUT,
            48,
            None,
            RuntimeStage::DisableScanout,
        );
        if result.is_err() {
            control.operation = None;
        }
        result
    }

    fn poll_update(&self) -> Result<Option<DisplayUpdate>, DisplayError> {
        let mut control = self.control.lock();
        let events = self
            .device
            .read_config_u32(VIRTIO_GPU_EVENTS_READ)
            .map_err(|_| DisplayError::Device)?;
        if events != 0 {
            self.device
                .write_config_u32(VIRTIO_GPU_EVENTS_CLEAR, events)
                .map_err(|_| DisplayError::Device)?;
            control.config_change_pending |= events & VIRTIO_GPU_EVENT_DISPLAY != 0;
        }
        let Some((head, _)) = control.queue.used().map_err(|()| DisplayError::Device)? else {
            if control.pending.is_none() && control.config_change_pending {
                control.config_change_pending = false;
                self.publish_display_info(&mut control)?;
            }
            return Ok(None);
        };
        let pending = control.pending.take().ok_or(DisplayError::Device)?;
        let expected_response = match pending.stage {
            RuntimeStage::DisplayInfo => VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            RuntimeStage::Create
            | RuntimeStage::Attach
            | RuntimeStage::TransferScanout
            | RuntimeStage::SetScanout
            | RuntimeStage::FlushScanout
            | RuntimeStage::UnrefReplaced
            | RuntimeStage::TransferDamage
            | RuntimeStage::FlushDamage
            | RuntimeStage::DisableScanout
            | RuntimeStage::UnrefDisabled => VIRTIO_GPU_RESP_OK_NODATA,
        };
        if head != pending.head
            || read_u32(&control.response, 0) != Some(expected_response)
            || read_u32(&control.response, 4).is_none_or(|flags| flags & VIRTIO_GPU_FLAG_FENCE == 0)
            || read_u64(&control.response, 8) != Some(pending.command_fence)
        {
            return Err(DisplayError::Device);
        }
        let update = match pending.stage {
            RuntimeStage::DisplayInfo => {
                let mode =
                    Self::parse_display_mode(&control.response).ok_or(DisplayError::Device)?;
                if mode == control.mode {
                    None
                } else {
                    control.mode = mode;
                    Some(DisplayUpdate::ModeChanged(mode))
                }
            }
            RuntimeStage::Create => {
                let (resource_id, backing) = match control.operation.as_ref() {
                    Some(RuntimeOperation::Scanout(transition)) => {
                        (transition.next.id, transition.next.backing.clone())
                    }
                    _ => return Err(DisplayError::Device),
                };
                let request_length = prepare_attach(&mut control.request, resource_id, &backing)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                    request_length,
                    Some(pending.operation_fence),
                    RuntimeStage::Attach,
                )?;
                None
            }
            RuntimeStage::Attach => {
                let (mode, resource_id) = match control.operation.as_ref() {
                    Some(RuntimeOperation::Scanout(transition)) => {
                        (transition.next.mode, transition.next.id)
                    }
                    _ => return Err(DisplayError::Device),
                };
                prepare_transfer(
                    &mut control.request,
                    mode,
                    DisplayRect {
                        x: 0,
                        y: 0,
                        width: mode.width,
                        height: mode.height,
                    },
                    resource_id,
                )?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                    56,
                    Some(pending.operation_fence),
                    RuntimeStage::TransferScanout,
                )?;
                None
            }
            RuntimeStage::TransferScanout => {
                let (mode, resource_id) = match control.operation.as_ref() {
                    Some(RuntimeOperation::Scanout(transition)) => {
                        (transition.next.mode, transition.next.id)
                    }
                    _ => return Err(DisplayError::Device),
                };
                prepare_set_scanout(&mut control.request, mode, resource_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_SET_SCANOUT,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::SetScanout,
                )?;
                None
            }
            RuntimeStage::SetScanout => {
                let (mode, resource_id) = match control.operation.as_ref() {
                    Some(RuntimeOperation::Scanout(transition)) => {
                        (transition.next.mode, transition.next.id)
                    }
                    _ => return Err(DisplayError::Device),
                };
                prepare_flush(
                    &mut control.request,
                    DisplayRect {
                        x: 0,
                        y: 0,
                        width: mode.width,
                        height: mode.height,
                    },
                    resource_id,
                )?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::FlushScanout,
                )?;
                None
            }
            RuntimeStage::FlushScanout => {
                if let Some(old_id) = control.active.as_ref().map(|resource| resource.id) {
                    prepare_unref(&mut control.request, old_id)?;
                    self.publish_runtime(
                        &mut control,
                        VIRTIO_GPU_CMD_RESOURCE_UNREF,
                        32,
                        Some(pending.operation_fence),
                        RuntimeStage::UnrefReplaced,
                    )?;
                    None
                } else {
                    let next = match control.operation.take() {
                        Some(RuntimeOperation::Scanout(transition)) => transition.next,
                        _ => return Err(DisplayError::Device),
                    };
                    control.active = Some(next);
                    if control.config_change_pending {
                        control.config_change_pending = false;
                        self.publish_display_info(&mut control)?;
                    }
                    return Ok(Some(DisplayUpdate::OperationCompleted(
                        pending.operation_fence,
                    )));
                }
            }
            RuntimeStage::UnrefReplaced => {
                let next = match control.operation.take() {
                    Some(RuntimeOperation::Scanout(transition)) => transition.next,
                    _ => return Err(DisplayError::Device),
                };
                let old = control.active.replace(next).ok_or(DisplayError::Device)?;
                control.release_resource_id(old.id)?;
                if control.config_change_pending {
                    control.config_change_pending = false;
                    self.publish_display_info(&mut control)?;
                }
                drop(control);
                // 最后一个 backing Arc 可能回收连续 extent；必须在 controlq lock 外析构，
                // 否则 frame allocator latency 会阻塞后续 IRQ completion。
                drop(old);
                return Ok(Some(DisplayUpdate::OperationCompleted(
                    pending.operation_fence,
                )));
            }
            RuntimeStage::TransferDamage => {
                let (rectangle, resource_id) =
                    match (control.operation.as_ref(), control.active.as_ref()) {
                        (Some(RuntimeOperation::Damage(damage)), Some(active)) => {
                            (damage.rectangles[usize::from(damage.index)], active.id)
                        }
                        _ => return Err(DisplayError::Device),
                    };
                prepare_flush(&mut control.request, rectangle, resource_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::FlushDamage,
                )?;
                None
            }
            RuntimeStage::FlushDamage => {
                let next_rectangle = match control.operation.as_mut() {
                    Some(RuntimeOperation::Damage(damage)) => {
                        damage.index += 1;
                        (damage.index < damage.count)
                            .then(|| damage.rectangles[usize::from(damage.index)])
                    }
                    _ => return Err(DisplayError::Device),
                };
                if let Some(rectangle) = next_rectangle {
                    let active = control.active.as_ref().ok_or(DisplayError::Device)?;
                    let mode = active.mode;
                    let resource_id = active.id;
                    prepare_transfer(&mut control.request, mode, rectangle, resource_id)?;
                    self.publish_runtime(
                        &mut control,
                        VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                        56,
                        Some(pending.operation_fence),
                        RuntimeStage::TransferDamage,
                    )?;
                    None
                } else {
                    match control.operation.take() {
                        Some(RuntimeOperation::Damage(_)) => {}
                        _ => return Err(DisplayError::Device),
                    }
                    if control.config_change_pending {
                        control.config_change_pending = false;
                        self.publish_display_info(&mut control)?;
                    }
                    return Ok(Some(DisplayUpdate::OperationCompleted(
                        pending.operation_fence,
                    )));
                }
            }
            RuntimeStage::DisableScanout => {
                let old_id = control
                    .active
                    .as_ref()
                    .map(|resource| resource.id)
                    .ok_or(DisplayError::Device)?;
                prepare_unref(&mut control.request, old_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_UNREF,
                    32,
                    Some(pending.operation_fence),
                    RuntimeStage::UnrefDisabled,
                )?;
                None
            }
            RuntimeStage::UnrefDisabled => {
                match control.operation.take() {
                    Some(RuntimeOperation::Disable) => {}
                    _ => return Err(DisplayError::Device),
                }
                let old = control.active.take().ok_or(DisplayError::Device)?;
                control.release_resource_id(old.id)?;
                if control.config_change_pending {
                    control.config_change_pending = false;
                    self.publish_display_info(&mut control)?;
                }
                drop(control);
                drop(old);
                return Ok(Some(DisplayUpdate::OperationCompleted(
                    pending.operation_fence,
                )));
            }
        };
        if control.pending.is_none() && control.config_change_pending {
            control.config_change_pending = false;
            self.publish_display_info(&mut control)?;
        }
        Ok(update)
    }
}
