use spin::Mutex;

use crate::memory::{DeviceBacking, PAGE_SIZE};

use super::{
    ATTACH_REQUEST_SIZE, CONTROL_QUEUE, ControlQueue, DisplayMode, VirtIODevice, VirtIOGpuDevice,
    wire::*,
};

impl VirtIOGpuDevice {
    pub(super) fn display_mode(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
    ) -> Option<DisplayMode> {
        control.lock().request[..CONTROL_HEADER_SIZE].fill(0);
        Self::execute_boot(
            device,
            control,
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            CONTROL_HEADER_SIZE,
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
        )?;
        Self::parse_display_mode(&control.lock().response[..])
    }

    pub(super) fn parse_display_mode(response: &[u8]) -> Option<DisplayMode> {
        for scanout in 0..16 {
            let offset = CONTROL_HEADER_SIZE + scanout * 24;
            let host_width = read_u32(response, offset + 8)?;
            let height = read_u32(response, offset + 12)?;
            let enabled = read_u32(response, offset + 16)?;
            // Linux virtio-gpu 把 display-info 宽度转换为 8-pixel granular CVT mode，后续
            // resource 与 SET_SCANOUT 也只消费该 mode；若保留 host 原始宽度，DRM 暴露的
            // hdisplay 与 adapter 校验会成为两套事实，非 8 对齐窗口永远返回 EINVAL。
            let width = host_width - host_width % 8;
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
        framebuffer: &DeviceBacking,
    ) -> Option<()> {
        {
            let mut control = control.lock();
            control.request[..40].fill(0);
            write_u32(control.request.as_mut_slice(), 24, BOOT_RESOURCE_ID)?;
            write_u32(
                control.request.as_mut_slice(),
                28,
                VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            )?;
            write_u32(control.request.as_mut_slice(), 32, mode.width)?;
            write_u32(control.request.as_mut_slice(), 36, mode.height)?;
        }
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, 40)?;

        let attach_length = 32 + framebuffer.extent_count() * 16;
        {
            let mut control = control.lock();
            control.request[..attach_length].fill(0);
            write_u32(control.request.as_mut_slice(), 24, BOOT_RESOURCE_ID)?;
            write_u32(
                control.request.as_mut_slice(),
                28,
                u32::try_from(framebuffer.extent_count()).ok()?,
            )?;
            for index in 0..framebuffer.extent_count() {
                let (ppn, pages) = framebuffer.extent(index)?;
                let offset = 32 + index * 16;
                write_u64(
                    control.request.as_mut_slice(),
                    offset,
                    (ppn.as_usize() * PAGE_SIZE) as u64,
                )?;
                write_u32(
                    control.request.as_mut_slice(),
                    offset + 8,
                    u32::try_from(pages.checked_mul(PAGE_SIZE)?).ok()?,
                )?;
            }
        }
        Self::execute_ok(
            device,
            control,
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
            attach_length,
        )?;

        {
            let mut control = control.lock();
            control.request[..48].fill(0);
            write_rect(control.request.as_mut_slice(), 24, mode)?;
            write_u32(control.request.as_mut_slice(), 40, 0)?;
            write_u32(control.request.as_mut_slice(), 44, BOOT_RESOURCE_ID)?;
        }
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_SET_SCANOUT, 48)?;

        {
            let mut control = control.lock();
            control.request[..56].fill(0);
            write_rect(control.request.as_mut_slice(), 24, mode)?;
            write_u64(control.request.as_mut_slice(), 40, 0)?;
            write_u32(control.request.as_mut_slice(), 48, BOOT_RESOURCE_ID)?;
        }
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, 56)?;

        {
            let mut control = control.lock();
            control.request[..48].fill(0);
            write_rect(control.request.as_mut_slice(), 24, mode)?;
            write_u32(control.request.as_mut_slice(), 40, BOOT_RESOURCE_ID)?;
        }
        Self::execute_ok(device, control, VIRTIO_GPU_CMD_RESOURCE_FLUSH, 48)
    }

    fn execute_ok(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
        command: u32,
        request_length: usize,
    ) -> Option<()> {
        Self::execute_boot(
            device,
            control,
            command,
            request_length,
            VIRTIO_GPU_RESP_OK_NODATA,
        )
    }

    fn execute_boot(
        device: &VirtIODevice,
        control: &Mutex<ControlQueue>,
        command: u32,
        request_length: usize,
        expected_response: u32,
    ) -> Option<()> {
        if !(CONTROL_HEADER_SIZE..=ATTACH_REQUEST_SIZE).contains(&request_length) {
            return None;
        }
        let mut control = control.lock();
        let fence = control.next_fence;
        control.next_fence = control.next_fence.checked_add(1)?;

        // 1. caller 提供固定 storage；common header 只在 descriptor 发布前写入。
        write_u32(control.request.as_mut_slice(), 0, command)?;
        write_u32(control.request.as_mut_slice(), 4, VIRTIO_GPU_FLAG_FENCE)?;
        write_u64(control.request.as_mut_slice(), 8, fence)?;
        control.response.fill(0);
        let head = {
            let ControlQueue {
                queue,
                request,
                response,
                ..
            } = &mut *control;
            let request = request.readable(0..request_length).ok()?;
            let response = response.writable_all();
            queue.add_dma(&[request, response]).ok()?
        };
        control.queue.add_to_avail(head);
        device.notify_queue(CONTROL_QUEUE).ok()?;

        // 2. 这是 scheduler/IRQ publication 前的唯一 bootstrap spin path。
        loop {
            match control.queue.used() {
                Ok(Some(completion))
                    if completion.head() == head
                        && completion.length() as usize
                            == if expected_response == VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
                                DISPLAY_INFO_SIZE
                            } else {
                                CONTROL_HEADER_SIZE
                            }
                        && read_u32(control.response.as_slice(), 0)? == expected_response
                        && read_u32(control.response.as_slice(), 4)? & VIRTIO_GPU_FLAG_FENCE
                            != 0
                        && read_u64(control.response.as_slice(), 8)? == fence =>
                {
                    control.queue.recycle_used(completion).ok()?;
                    break;
                }
                Ok(Some(_)) | Err(()) => return None,
                Ok(None) => core::hint::spin_loop(),
            }
        }
        if let Ok(status) = device.interrupt_status()
            && status != 0
        {
            device.interrupt_ack(status).ok()?;
        }
        Some(())
    }
}
