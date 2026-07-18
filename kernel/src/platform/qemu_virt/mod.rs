//! @description QEMU `virt` platform implementation。

#[macro_use]
pub(crate) mod console;
mod devices;
mod discovery;
mod firmware;
mod plic;
mod plic_policy;

pub(crate) use devices::{handle_external_interrupt, initialize as initialize_devices};
pub(crate) use discovery::{BootInfo, hardware_cpu_ids, initialize, validate_boot_info};
pub(crate) use firmware::{
    ResetError, TlbShootdownError, arm_timer, debug_console_write, reset_system, send_ipi,
    start_cpu, synchronize_tlb, verify_firmware,
};

/// @description 投影 platform 可分配 physical memory 的 exclusive end。
/// @return 已验证 DTB memory range 的 end address。
pub(crate) fn physical_memory_end() -> usize {
    discovery::info().mem.end
}

/// @description 投影 architecture counter 的 platform frequency。
/// @return DTB `timebase-frequency`，零值由 timer owner fail-stop。
pub(crate) fn timebase_frequency() -> u64 {
    discovery::info().time_base_freq
}

/// @description 枚举 kernel address space 必须 identity-map 的 platform MMIO regions。
/// @return UART、VirtIO window、RTC 与 PLIC 的非空区间；concrete device facts 不穿过 seam。
pub(crate) fn kernel_mmio_regions() -> impl Iterator<Item = core::ops::Range<usize>> {
    let info = discovery::info();
    let mut regions = [None, None, None, None];
    if !info.uart.is_empty() {
        regions[0] = Some(info.uart.clone());
    }
    let mut virtio_start = usize::MAX;
    let mut virtio_end = 0usize;
    for device in info.virtio_devices[..info.virtio_count].iter().flatten() {
        virtio_start = virtio_start.min(device.base_addr);
        virtio_end = virtio_end.max(
            device
                .base_addr
                .checked_add(device.size)
                .expect("validated VirtIO MMIO range overflowed"),
        );
    }
    if virtio_start < virtio_end {
        regions[1] = Some(virtio_start..virtio_end);
    }
    regions[2] = info.rtc_device.map(|device| {
        device.base_addr
            ..device
                .base_addr
                .checked_add(device.size)
                .expect("validated RTC MMIO range overflowed")
    });
    regions[3] = info.plic_device.map(|device| {
        device.base_addr
            ..device
                .base_addr
                .checked_add(device.size)
                .expect("validated PLIC MMIO range overflowed")
    });
    regions.into_iter().flatten()
}

/// @description 从 platform realtime source 读取一次 Unix epoch 纳秒值。
/// @return RTC 存在且 MMIO read 成功时返回时间，否则返回 `None`。
pub(crate) fn read_realtime_ns() -> Option<u64> {
    let resource = discovery::info().rtc_device?;
    crate::drivers::GoldfishRTCDevice::new(resource.base_addr, resource.size)
        .ok()?
        .read_time_ns()
        .ok()
}
