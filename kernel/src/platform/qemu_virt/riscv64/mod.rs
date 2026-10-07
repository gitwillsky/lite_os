//! QEMU `virt` RISC-V backend implementation。

#[macro_use]
pub(crate) mod console;
mod device_tree;
mod devices;
mod discovery;
mod firmware;
mod plic;
mod plic_policy;
mod rtc;
mod uart;

pub(crate) use devices::{handle_external_interrupt, initialize as initialize_devices};
pub(crate) use discovery::{BootInfo, hardware_cpu_ids, initialize, validate_boot_info};
pub(crate) use firmware::{
    InstructionFenceError, ResetError, TlbShootdownError, arm_timer, debug_console_write,
    debug_console_write_bytes, reset_system, send_ipi, start_cpu, synchronize_instruction_cache,
    synchronize_tlb, verify_firmware,
};

/// claim 并处理当前 RISC-V external interrupt batch。
pub(crate) fn claim_interrupt() -> super::ClaimedInterrupt {
    handle_external_interrupt();
    super::ClaimedInterrupt::Device(0)
}

/// PLIC handler 已在 batch 内 exactly-once complete；此处只消费 typed token。
pub(crate) fn complete_interrupt(claim: super::ClaimedInterrupt) {
    assert!(
        matches!(claim, super::ClaimedInterrupt::Device(_)),
        "RISC-V PLIC returned a non-device semantic interrupt"
    );
    let _ = claim.completion_token();
}

/// 触发 calling hart 的 local supervisor software interrupt。
pub(crate) fn notify_self() {
    crate::arch::interrupt::raise_software();
}

/// RISC-V SBI RFENCE 由 firmware trap owner 完成，不使用 platform SGI mailbox。
pub(crate) fn complete_pending_ipi() {}

/// 投影 platform 可分配 physical memory 的 exclusive end。
///
/// # Returns
///
/// 已验证 DTB memory range 的 end address。
pub(crate) fn physical_memory_end() -> usize {
    discovery::info().memory.end
}

/// 投影 architecture counter 的 platform frequency。
///
/// # Returns
///
/// DTB `timebase-frequency`，零值由 timer owner fail-stop。
pub(crate) fn timebase_frequency() -> u64 {
    discovery::info().timebase_frequency
}

/// 枚举 kernel address space 必须 identity-map 的 platform MMIO regions。
///
/// # Returns
///
/// UART、VirtIO window、RTC 与 PLIC 的非空区间；concrete device facts 不穿过 seam。
pub(crate) fn kernel_mmio_regions() -> impl Iterator<Item = core::ops::Range<usize>> {
    let info = discovery::info();
    [
        Some(info.uart.clone()),
        info.virtio.span(),
        info.rtc.clone(),
        Some(info.plic.clone()),
    ]
    .into_iter()
    .flatten()
}

/// 从 platform realtime source 读取一次 Unix epoch 纳秒值。
///
/// # Returns
///
/// RTC 存在且 MMIO read 成功时返回时间，否则返回 `None`。
pub(crate) fn read_realtime_ns() -> Option<u64> {
    let resource = discovery::info().rtc.clone()?;
    rtc::GoldfishRTCDevice::new(resource.start, resource.end - resource.start)
        .ok()?
        .read_time_ns()
        .ok()
}

/// firmware 交付的 kernel command line（DTB `/chosen/bootargs`）。
pub(crate) fn kernel_command_line() -> &'static [u8] {
    &discovery::info().bootargs
}
