use spin::Mutex;

use crate::memory::{FrameTracker, PAGE_SIZE};

use super::{CONTROL_QUEUE, ControlQueue, DisplayMode, VirtIODevice, VirtIOGpuDevice, wire::*};

impl VirtIOGpuDevice {
    pub(super) fn display_mode(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
    ) -> Option<DisplayMode> {
        let mut response = [0u8; DISPLAY_INFO_SIZE];
        Self::execute_boot(
            device,
            control,
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            &mut [0u8; CONTROL_HEADER_SIZE],
            &mut response,
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        )?;
        Self::parse_display_mode(&response)
    }

    pub(super) fn parse_display_mode(response: &[u8]) -> Option<DisplayMode> {
        for scanout in 0..16 {
            let offset = CONTROL_HEADER_SIZE + scanout * 24;
            let width = read_u32(response, offset + 8)?;
            let height = read_u32(response, offset + 12)?;
            let enabled = read_u32(response, offset + 16)?;
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

    pub(super) fn initialize_scanout(
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

        // 1. caller 提供固定 storage；common header 只在 descriptor 发布前写入。
        write_u32(request, 0, command)?;
        write_u32(request, 4, VIRTIO_GPU_FLAG_FENCE)?;
        write_u64(request, 8, fence)?;
        response.fill(0);
        let mut outputs = [response];
        let head = control.queue.add_buffer(&[request], &mut outputs)?;
        control.queue.add_to_avail(head);
        device.notify_queue(CONTROL_QUEUE).ok()?;

        // 2. 这是 scheduler/IRQ publication 前的唯一 bootstrap spin path。
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

        // 3. response type 与 fence 共同证明 completion 属于本次 command。
        (read_u32(outputs[0], 0)? == expected_response
            && read_u32(outputs[0], 4)? & VIRTIO_GPU_FLAG_FENCE != 0
            && read_u64(outputs[0], 8)? == fence)
            .then_some(())
    }
}
