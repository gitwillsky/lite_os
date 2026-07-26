//! @description QEMU `virt` AArch64 machine backend。

use core::fmt;

#[macro_use]
pub(crate) mod console;
mod devices;
mod discovery;
mod gicv3;
mod pl011;
mod psci;

pub(crate) use devices::initialize as initialize_devices;
pub(crate) use discovery::{BootInfo, hardware_cpu_ids};
pub(crate) use gicv3::{claim_interrupt, complete_interrupt, notify_self, send_ipi};
pub(crate) use psci::{ResetError, reset_system, start_cpu};

#[derive(Debug, Clone, Copy)]
pub(crate) struct TimerArmError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TlbShootdownError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InstructionFenceError;

impl fmt::Display for TlbShootdownError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str("AArch64 TLB broadcast failed")
    }
}

impl fmt::Display for InstructionFenceError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str("AArch64 instruction publication failed")
    }
}

pub(crate) fn initialize(boot: BootInfo) {
    discovery::initialize(boot);
    console::validate_discovered_base();
}

pub(crate) fn validate_boot_info(boot: BootInfo) {
    discovery::validate_boot_info(boot);
    // secondary 只能在 boot CPU 发布 GIC global state 后执行本地 redistributor/ICC 初始化。
    gicv3::initialize_local();
}

pub(crate) fn verify_firmware() {
    psci::verify();
}

pub(crate) fn debug_console_write(byte: u8) -> Result<(), console::ConsoleError> {
    console::write_byte(byte)
}

pub(crate) fn physical_memory_end() -> usize {
    discovery::info().memory.end
}

pub(crate) fn timebase_frequency() -> u64 {
    crate::arch::time::counter_frequency()
}

pub(crate) fn kernel_mmio_regions() -> impl Iterator<Item = core::ops::Range<usize>> {
    let info = discovery::info();
    let mut virtio_start = usize::MAX;
    let mut virtio_end = 0usize;
    for device in info.virtio_devices[..info.virtio_count].iter().flatten() {
        virtio_start = virtio_start.min(device.base_addr);
        virtio_end = virtio_end.max(
            device
                .base_addr
                .checked_add(device.size)
                .expect("validated VirtIO range overflow"),
        );
    }
    [
        Some(info.uart.base_addr..info.uart.base_addr + info.uart.size),
        Some(info.rtc.range()),
        Some(info.gic.distributor.range()),
        Some(info.gic.redistributor.range()),
        (virtio_start < virtio_end).then_some(virtio_start..virtio_end),
    ]
    .into_iter()
    .flatten()
}

pub(crate) fn read_realtime_ns() -> Option<u64> {
    let rtc = discovery::info().rtc;
    if rtc.size < core::mem::size_of::<u32>() {
        return None;
    }
    // SAFETY: discovery verified PL031 compatibility and a permanent MMIO range containing RTCDR.
    let rtc_base = crate::arch::mmu::physical_to_virtual(rtc.start);
    let seconds = unsafe { core::ptr::read_volatile(rtc_base as *const u32) };
    Some((seconds as u64).saturating_mul(1_000_000_000))
}

pub(crate) fn arm_timer(deadline: u64) -> Result<(), TimerArmError> {
    crate::arch::time::program_virtual_timer(deadline);
    Ok(())
}

/// @description 同步完成 AArch64 `virt` inner-shareable domain 的远端 translation fence。
/// @param cpus 至少一个可能持有 stale translation 的 logical CPU；空集合无需硬件操作。
/// @param start_address generic owner 归一化的起始地址；本 backend 为可靠性升级为 full fence。
/// @param size generic owner 归一化的区间长度；本 backend 为可靠性升级为 full fence。
/// @return `VMALLE1IS` completion 后返回成功；当前 backend 没有可恢复失败。
pub(crate) fn synchronize_tlb(
    cpus: crate::cpu::CpuSet,
    start_address: usize,
    size: usize,
) -> Result<(), TlbShootdownError> {
    if cpus.is_empty() {
        return Ok(());
    }
    // 1. Apple HVF 下 VA/ASID-scoped inner-shareable TLBI 不能可靠清除其他 vCPU 的 writable
    // translation；保留 range 会让迁移后的 task 绕过 COW fault 并写坏共享用户页。
    // 2. VMALLE1IS 是该 platform 实测可靠的唯一同步原语。它只用于 Revoke/retirement，
    // Publish/Relax 热路径仍保持 local fence，因此不会把每次 page fault 扩成全局 shootdown。
    let _ = (start_address, size);
    crate::arch::mmu::broadcast_tlb(0, 0);
    Ok(())
}

pub(crate) fn synchronize_instruction_cache(
    cpus: crate::cpu::CpuSet,
) -> Result<(), InstructionFenceError> {
    if cpus.is_empty() {
        return Ok(());
    }
    crate::arch::instruction::broadcast_instruction_cache();
    Ok(())
}
