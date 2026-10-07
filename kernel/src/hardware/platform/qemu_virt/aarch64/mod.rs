//! QEMU `virt` AArch64 machine backend。

use core::fmt;

#[macro_use]
pub(crate) mod console;
mod device_tree;
mod devices;
mod discovery;
mod gicv3;
mod pl011;
mod psci;
mod tlb_shootdown;

pub(crate) use discovery::{BootInfo, hardware_cpu_ids};
pub(crate) use gicv3::{claim_interrupt, complete_interrupt, send_ipi};
pub(crate) use psci::{ResetError, reset_system, start_cpu};

#[derive(Debug, Clone, Copy)]
pub(crate) struct TimerArmError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TlbShootdownError;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InstructionFenceError;

impl fmt::Display for TlbShootdownError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str("AArch64 TLB rendezvous failed")
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

/// 初始化 AArch64 interrupt controller 与逐 CPU TLB rendezvous state。
///
/// # Errors
///
/// controller 或 rendezvous 初始化失败时 fail-stop。
pub(crate) use devices::{
    map_device_window, pci_host, register_device_interrupt, virtio_mmio_transports,
};

pub(crate) fn initialize_devices() {
    devices::initialize();
    tlb_shootdown::initialize();
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
    [
        Some(info.uart.base_addr..info.uart.base_addr + info.uart.size),
        Some(info.rtc.range()),
        Some(info.gic.distributor.range()),
        Some(info.gic.redistributor.range()),
        info.virtio.span(),
        Some(info.pci.ecam.clone()),
        Some(info.pci.mmio32.clone()),
    ]
    .into_iter()
    .flatten()
}

pub(crate) fn read_realtime_ns() -> Option<u64> {
    let rtc = discovery::info().rtc;
    // discovery 已验证 PL031 compatibility；RTCDR 位于 window 偏移 0。
    let bus = crate::hal::MmioBus::new(crate::arch::mmu::physical_to_virtual(rtc.start), rtc.size)
        .ok()?;
    let seconds = bus.read_u32(0).ok()?;
    Some((seconds as u64).saturating_mul(1_000_000_000))
}

pub(crate) fn arm_timer(deadline: u64) -> Result<(), TimerArmError> {
    crate::arch::time::program_virtual_timer(deadline);
    Ok(())
}

/// 广播 full translation fence，并等待每颗 AArch64 `virt` 目标 vCPU 越过 flush point。
///
/// # Parameters
///
/// - `cpus`: 至少一个可能持有 stale translation 的 logical CPU；空集合无需硬件操作。
/// - `start_address`: generic owner 归一化的起始地址；本 backend 为可靠性升级为 full broadcast。
/// - `size`: generic owner 归一化的区间长度；本 backend 为可靠性升级为 full broadcast。
///
/// # Returns
///
/// source `VMALLE1IS` 完成且每颗目标 vCPU 的 SGI handler 发布 ack 后成功。
///
/// # Errors
///
/// SGI 投递失败时返回 `TlbShootdownError`。
pub(crate) fn synchronize_tlb(
    cpus: crate::cpu::CpuSet,
    start_address: usize,
    size: usize,
) -> Result<(), TlbShootdownError> {
    if cpus.is_empty() {
        return Ok(());
    }
    // Apple HVF 的 VMALLE1IS 返回不能证明每颗 vCPU 都已越过 hypervisor flush point；
    // full broadcast 后必须逐目标取得 SGI ack，才能释放 COW frame 或 kernel stack。
    let _ = (start_address, size);
    crate::arch::mmu::broadcast_tlb();
    tlb_shootdown::synchronize(cpus)
}

/// 在 AArch64 SGI completion seam 消费当前 vCPU 的 TLB request。
///
/// # Returns
///
/// 没有新 request 时不执行操作。
pub(crate) fn complete_pending_ipi() {
    tlb_shootdown::complete_pending();
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

/// firmware 交付的 kernel command line（DTB `/chosen/bootargs`）。
pub(crate) fn kernel_command_line() -> &'static [u8] {
    &discovery::info().bootargs
}
