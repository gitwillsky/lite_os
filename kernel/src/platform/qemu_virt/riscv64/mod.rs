//! QEMU `virt` RISC-V backend implementation。

#[macro_use]
pub(crate) mod console;
mod aia;
mod device_tree;
mod devices;
mod discovery;
mod firmware;
mod rtc;
mod uart;

pub(crate) use aia::{
    InterruptRetirement, MessageInterruptGroup, MessageInterruptRoute, bind_interrupt,
    bind_message_interrupt, device_interrupt_range, handle_external_interrupt,
    set_interrupt_affinity, unbind_interrupt, unmask_interrupt,
};
pub(crate) use devices::initialize as initialize_devices;
pub(crate) use discovery::{BootInfo, hardware_cpu_ids, initialize};
pub(crate) fn validate_boot_info(boot: BootInfo) {
    discovery::validate_boot_info(boot);
    aia::initialize_local();
}
pub(crate) use firmware::{
    InstructionFenceError, TlbShootdownError, arm_timer, debug_console_write,
    debug_console_write_bytes, reset_system, send_ipi, start_cpu, synchronize_instruction_cache,
    synchronize_tlb, verify_firmware,
};

/// claim 并处理当前 RISC-V external interrupt batch。
pub(crate) fn claim_interrupt() -> super::ClaimedInterrupt {
    handle_external_interrupt();
    super::ClaimedInterrupt::Device(0)
}

/// IMSIC claim 已在 batch 内 exactly-once complete；此处只消费 typed token。
pub(crate) fn complete_interrupt(claim: super::ClaimedInterrupt) {
    assert!(
        matches!(claim, super::ClaimedInterrupt::Device(_)),
        "RISC-V IMSIC returned a non-device semantic interrupt"
    );
    let _ = claim.completion_token();
}

/// 消费本 CPU IMSIC CSR/marker 请求；SBI RFENCE 仍由 M-mode firmware 确认。
pub(crate) use aia::complete_pending as complete_pending_ipi;

/// 投影 platform 可分配 physical memory 的 exclusive end。
///
/// # Returns
///
/// 已验证 DTB memory range 的 end address。
/// 平台 RAM 的物理区间。
pub(crate) fn physical_memory_range() -> core::ops::Range<usize> {
    discovery::info().memory.clone()
}

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
/// UART、RTC、APLIC 与 IMSIC 的非空区间；concrete device facts 不穿过 seam。
pub(crate) fn kernel_mmio_regions() -> impl Iterator<Item = core::ops::Range<usize>> {
    let info = discovery::info();
    [
        Some(info.uart.clone()),
        info.rtc.clone(),
        Some(info.aia.aplic.clone()),
    ]
    .into_iter()
    .flatten()
    .chain(info.aia.windows.iter().cloned())
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
/// initrd 物理区间（DT `/chosen`）；没有 initrd 时为 `None`。
pub(crate) fn initrd_range() -> Option<core::ops::Range<usize>> {
    discovery::info().initrd.clone()
}

/// 固件提供的启动熵（DT `/chosen/rng-seed`）。
pub(crate) fn boot_entropy() -> &'static [u8] {
    &discovery::info().rng_seed
}

/// DTB 自身占用的物理区间；交给用户态发现其余设备。
pub(crate) fn dtb_range() -> core::ops::Range<usize> {
    discovery::info().dtb.clone()
}

pub(crate) fn kernel_command_line() -> &'static [u8] {
    &discovery::info().bootargs
}
