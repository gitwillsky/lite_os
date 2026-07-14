use alloc::sync::Arc;
use spin::Mutex;

use crate::memory::{FrameAllocationClass, FrameTracker, PAGE_SIZE, alloc_contiguous};

use super::{
    DisplayDevice, DisplayError, DisplayMode, InterruptError, InterruptHandler, InterruptVector,
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice, virtio_queue::VirtQueue,
};

mod wire;
use wire::*;

const CONTROL_QUEUE: u32 = 0;
const QUEUE_SIZE: u16 = 64;

#[derive(Clone, Copy)]
enum RuntimeStage {
    Create,
    Attach,
    Transfer,
    SetScanout,
    Flush,
    Unref,
}

struct ScanoutResource {
    id: u32,
    // OWNER: resource backing 必须活到 RESOURCE_UNREF completion；只记录 PPN 会让
    // userspace close/RMFB 后 allocator 重用 device 仍可 DMA 的 extent。
    backing: Arc<FrameTracker>,
}

struct ScanoutTransition {
    // OWNER: next resource 从 CREATE publication 到最终成为 active 始终由 controlq
    // transaction 保活；缺失该字段会在多阶段 IRQ 间隙释放 backing。
    next: ScanoutResource,
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
    request: [u8; 56],
    response: [u8; CONTROL_HEADER_SIZE],
    // OWNER: pending 是 descriptor head、command fence 与 stage 的唯一对应关系；若按
    // command type 分散记录，乱序或 stale completion 会推进错误 transaction。
    pending: Option<PendingCommand>,
    // OWNER: active resource 保活当前 hardware scanout；只在旧 resource UNREF completion
    // 后替换，避免 device 与 allocator 并发访问同一物理 extent。
    active: Option<ScanoutResource>,
    // OWNER: transition 串联 CREATE→ATTACH→TRANSFER→SET→FLUSH→UNREF；缺失时每个 IRQ
    // stage 无法证明 backing 与 operation fence 属于同一次 page flip。
    transition: Option<ScanoutTransition>,
    // 两个 resource ID 交替复用；只有 UNREF completion 后才归还。提前复用会让 device
    // 把新 command 误绑定到仍存在的旧 resource。
    reusable_resource_id: Option<u32>,
}

/// @description VirtIO-GPU 2D single-scanout adapter。
pub(super) struct VirtIOGpuDevice {
    device: VirtIODevice,
    // OWNER: adapter 在 device ready 后永久持有 controlq DMA backing；若初始化后释放，
    // device 仍可访问已经归还 allocator 的 descriptor pages。
    control: Mutex<ControlQueue>,
    mode: DisplayMode,
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
            request: [0; 56],
            response: [0; CONTROL_HEADER_SIZE],
            pending: None,
            active: None,
            transition: None,
            reusable_resource_id: None,
        });
        let mode = Self::display_mode(&device, &control)?;
        let framebuffer_bytes = usize::try_from(mode.pitch)
            .ok()?
            .checked_mul(usize::try_from(mode.height).ok()?)?;
        let framebuffer = Arc::try_new(alloc_contiguous(
            framebuffer_bytes.div_ceil(PAGE_SIZE),
            FrameAllocationClass::KernelCritical,
        )?)
        .ok()?;
        Self::initialize_scanout(&device, &control, mode, &framebuffer, framebuffer_bytes)?;

        {
            let mut control = control.lock();
            control.active = Some(ScanoutResource {
                id: BOOT_RESOURCE_ID,
                backing: framebuffer,
            });
            control.reusable_resource_id = Some(ALTERNATE_RESOURCE_ID);
        }

        Arc::try_new(Self {
            device,
            control,
            mode,
        })
        .ok()
    }

    fn display_mode(device: &VirtIODevice, control: &Mutex<ControlQueue>) -> Option<DisplayMode> {
        let mut response = [0u8; DISPLAY_INFO_SIZE];
        Self::execute_boot(
            device,
            control,
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            &mut [0u8; CONTROL_HEADER_SIZE],
            &mut response,
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        )?;
        for scanout in 0..16 {
            let offset = CONTROL_HEADER_SIZE + scanout * 24;
            let width = read_u32(&response, offset + 8)?;
            let height = read_u32(&response, offset + 12)?;
            let enabled = read_u32(&response, offset + 16)?;
            if enabled != 0 && width != 0 && height != 0 {
                return Some(DisplayMode {
                    width,
                    height,
                    pitch: width.checked_mul(4)?,
                });
            }
        }
        None
    }

    fn initialize_scanout(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
        mode: DisplayMode,
        framebuffer: &FrameTracker,
        framebuffer_bytes: usize,
    ) -> Option<()> {
        let mut create = [0u8; 40];
        write_u32(&mut create, 24, BOOT_RESOURCE_ID)?;
        write_u32(&mut create, 28, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM)?;
        write_u32(&mut create, 32, mode.width)?;
        write_u32(&mut create, 36, mode.height)?;
        Self::execute_ok(
            device,
            control,
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
            &mut create,
        )?;

        let mut attach = [0u8; 48];
        write_u32(&mut attach, 24, BOOT_RESOURCE_ID)?;
        write_u32(&mut attach, 28, 1)?;
        write_u64(
            &mut attach,
            32,
            (framebuffer.ppn.as_usize() * PAGE_SIZE) as u64,
        )?;
        write_u32(&mut attach, 40, u32::try_from(framebuffer_bytes).ok()?)?;
        Self::execute_ok(
            device,
            control,
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
            &mut attach,
        )?;

        let mut scanout = [0u8; 48];
        write_rect(&mut scanout, 24, mode)?;
        write_u32(&mut scanout, 40, 0)?;
        write_u32(&mut scanout, 44, BOOT_RESOURCE_ID)?;
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_SET_SCANOUT, &mut scanout)?;

        let mut transfer = [0u8; 56];
        write_rect(&mut transfer, 24, mode)?;
        write_u64(&mut transfer, 40, 0)?;
        write_u32(&mut transfer, 48, BOOT_RESOURCE_ID)?;
        Self::execute_ok(
            device,
            control,
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
            &mut transfer,
        )?;

        let mut flush = [0u8; 48];
        write_rect(&mut flush, 24, mode)?;
        write_u32(&mut flush, 40, BOOT_RESOURCE_ID)?;
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_RESOURCE_FLUSH, &mut flush)
    }

    fn execute_ok(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
        command: u32,
        request: &mut [u8],
    ) -> Option<()> {
        let mut response = [0u8; CONTROL_HEADER_SIZE];
        Self::execute_boot(
            device,
            control,
            command,
            request,
            &mut response,
            VIRTIO_GPU_RESP_OK_NODATA,
        )
    }

    fn execute_boot(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
        command: u32,
        request: &mut [u8],
        response: &mut [u8],
        expected_response: u32,
    ) -> Option<()> {
        if request.len() < CONTROL_HEADER_SIZE || response.len() < CONTROL_HEADER_SIZE {
            return None;
        }
        let mut control = control.lock();
        let fence = control.next_fence;
        control.next_fence = control.next_fence.checked_add(1)?;

        // 1. caller 为命令体提供固定大小 storage；只在 descriptor 发布前
        //    写入 common header，避免为不同 GPU command 复制 queue lifecycle。
        write_u32(request, 0, command)?;
        write_u32(request, 4, VIRTIO_GPU_FLAG_FENCE)?;
        write_u64(request, 8, fence)?;
        response.fill(0);

        let mut outputs = [response];
        let head = control.queue.add_buffer(&[request], &mut outputs)?;
        control.queue.add_to_avail(head);
        device.notify_queue(CONTROL_QUEUE).ok()?;

        // 2. 这是 scheduler/IRQ publication 之前的唯一 bootstrap transaction；runtime DRM
        //    command 不得复用该路径，否则 page flip 会重新占用 CPU 忙等。
        loop {
            match control.queue.used() {
                Ok(Some((completed, _))) if completed == head => break,
                Ok(Some(_)) | Err(()) => return None,
                Ok(None) => core::hint::spin_loop(),
            }
        }
        if let Ok(status) = device.interrupt_status()
            && status != 0
        {
            device.interrupt_ack(status).ok()?;
        }

        // 3. fence 同时证明 response 属于本次 command 且 device 已完成指定操作。
        (read_u32(outputs[0], 0)? == expected_response
            && read_u32(outputs[0], 4)? & VIRTIO_GPU_FLAG_FENCE != 0
            && read_u64(outputs[0], 8)? == fence)
            .then_some(())
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
        control.next_fence = control
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
        queue.add_to_avail(head);
        let operation_fence = operation_fence.unwrap_or(command_fence);
        control.pending = Some(PendingCommand {
            head,
            operation_fence,
            command_fence,
            stage,
        });
        self.device
            .notify_queue(CONTROL_QUEUE)
            .map_err(|_| DisplayError::Device)?;
        Ok(operation_fence)
    }

    fn prepare_create(
        request: &mut [u8],
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<(), DisplayError> {
        request.fill(0);
        write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
        write_u32(request, 28, VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM).ok_or(DisplayError::Device)?;
        write_u32(request, 32, mode.width).ok_or(DisplayError::Device)?;
        write_u32(request, 36, mode.height).ok_or(DisplayError::Device)
    }

    fn prepare_attach(
        request: &mut [u8],
        resource_id: u32,
        physical_address: u64,
        bytes: usize,
    ) -> Result<(), DisplayError> {
        request.fill(0);
        write_u32(request, 24, resource_id).ok_or(DisplayError::Device)?;
        write_u32(request, 28, 1).ok_or(DisplayError::Device)?;
        write_u64(request, 32, physical_address).ok_or(DisplayError::Device)?;
        write_u32(
            request,
            40,
            u32::try_from(bytes).map_err(|_| DisplayError::InvalidRectangle)?,
        )
        .ok_or(DisplayError::Device)
    }

    fn prepare_transfer(
        request: &mut [u8],
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<(), DisplayError> {
        request.fill(0);
        write_rect(request, 24, mode).ok_or(DisplayError::InvalidRectangle)?;
        write_u64(request, 40, 0).ok_or(DisplayError::Device)?;
        write_u32(request, 48, resource_id).ok_or(DisplayError::Device)
    }

    fn prepare_set_scanout(
        request: &mut [u8],
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<(), DisplayError> {
        request.fill(0);
        write_rect(request, 24, mode).ok_or(DisplayError::InvalidRectangle)?;
        write_u32(request, 40, 0).ok_or(DisplayError::Device)?;
        write_u32(request, 44, resource_id).ok_or(DisplayError::Device)
    }

    fn prepare_flush(
        request: &mut [u8],
        mode: DisplayMode,
        resource_id: u32,
    ) -> Result<(), DisplayError> {
        request.fill(0);
        write_rect(request, 24, mode).ok_or(DisplayError::InvalidRectangle)?;
        write_u32(request, 40, resource_id).ok_or(DisplayError::Device)
    }

    fn prepare_unref(request: &mut [u8], resource_id: u32) -> Result<(), DisplayError> {
        request.fill(0);
        write_u32(request, 24, resource_id).ok_or(DisplayError::Device)
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
        if status & VIRTIO_MMIO_INT_VRING != 0 {
            crate::arch::hart::raise_display_softirq();
        }
        Ok(())
    }
}

impl DisplayDevice for VirtIOGpuDevice {
    fn mode(&self) -> DisplayMode {
        self.mode
    }

    fn initial_backing(&self) -> Arc<FrameTracker> {
        self.control
            .lock()
            .active
            .as_ref()
            .expect("VirtIO GPU initialized without active resource")
            .backing
            .clone()
    }

    fn submit_scanout(&self, backing: Arc<FrameTracker>) -> Result<u64, DisplayError> {
        let bytes = usize::try_from(self.mode.pitch)
            .ok()
            .and_then(|pitch| pitch.checked_mul(self.mode.height as usize))
            .ok_or(DisplayError::InvalidRectangle)?;
        if backing
            .pages
            .checked_mul(PAGE_SIZE)
            .is_none_or(|capacity| capacity < bytes)
            || u32::try_from(bytes).is_err()
        {
            return Err(DisplayError::InvalidRectangle);
        }
        let mut control = self.control.lock();
        if control.pending.is_some() || control.transition.is_some() {
            return Err(DisplayError::WouldBlock);
        }
        let resource_id = control
            .reusable_resource_id
            .take()
            .ok_or(DisplayError::Device)?;
        control.transition = Some(ScanoutTransition {
            next: ScanoutResource {
                id: resource_id,
                backing,
            },
        });
        Self::prepare_create(&mut control.request, self.mode, resource_id)?;
        self.publish_runtime(
            &mut control,
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
            40,
            None,
            RuntimeStage::Create,
        )
    }

    fn poll_completion(&self) -> Result<Option<u64>, DisplayError> {
        let mut control = self.control.lock();
        let Some((head, _)) = control.queue.used().map_err(|()| DisplayError::Device)? else {
            return Ok(None);
        };
        let pending = control.pending.take().ok_or(DisplayError::Device)?;
        if head != pending.head
            || read_u32(&control.response, 0) != Some(VIRTIO_GPU_RESP_OK_NODATA)
            || read_u32(&control.response, 4).is_none_or(|flags| flags & VIRTIO_GPU_FLAG_FENCE == 0)
            || read_u64(&control.response, 8) != Some(pending.command_fence)
        {
            return Err(DisplayError::Device);
        }
        let resource_id = control
            .transition
            .as_ref()
            .map(|transition| transition.next.id)
            .ok_or(DisplayError::Device)?;
        match pending.stage {
            RuntimeStage::Create => {
                let bytes = usize::try_from(self.mode.pitch)
                    .ok()
                    .and_then(|pitch| pitch.checked_mul(self.mode.height as usize))
                    .ok_or(DisplayError::InvalidRectangle)?;
                let physical_address = control
                    .transition
                    .as_ref()
                    .map(|transition| (transition.next.backing.ppn.as_usize() * PAGE_SIZE) as u64)
                    .ok_or(DisplayError::Device)?;
                Self::prepare_attach(&mut control.request, resource_id, physical_address, bytes)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::Attach,
                )?;
                Ok(None)
            }
            RuntimeStage::Attach => {
                Self::prepare_transfer(&mut control.request, self.mode, resource_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                    56,
                    Some(pending.operation_fence),
                    RuntimeStage::Transfer,
                )?;
                Ok(None)
            }
            RuntimeStage::Transfer => {
                Self::prepare_set_scanout(&mut control.request, self.mode, resource_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_SET_SCANOUT,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::SetScanout,
                )?;
                Ok(None)
            }
            RuntimeStage::SetScanout => {
                Self::prepare_flush(&mut control.request, self.mode, resource_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                    48,
                    Some(pending.operation_fence),
                    RuntimeStage::Flush,
                )?;
                Ok(None)
            }
            RuntimeStage::Flush => {
                let old_id = control
                    .active
                    .as_ref()
                    .map(|resource| resource.id)
                    .ok_or(DisplayError::Device)?;
                Self::prepare_unref(&mut control.request, old_id)?;
                self.publish_runtime(
                    &mut control,
                    VIRTIO_GPU_CMD_RESOURCE_UNREF,
                    32,
                    Some(pending.operation_fence),
                    RuntimeStage::Unref,
                )?;
                Ok(None)
            }
            RuntimeStage::Unref => {
                let transition = control.transition.take().ok_or(DisplayError::Device)?;
                let old = control
                    .active
                    .replace(transition.next)
                    .ok_or(DisplayError::Device)?;
                if control.reusable_resource_id.replace(old.id).is_some() {
                    return Err(DisplayError::Device);
                }
                drop(control);
                // 最后一个 backing Arc 可能回收连续 extent；必须在 controlq lock 外析构，
                // 否则 frame allocator latency 会阻塞后续 IRQ completion。
                drop(old);
                Ok(Some(pending.operation_fence))
            }
        }
    }
}
