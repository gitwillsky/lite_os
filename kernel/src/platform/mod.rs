//! 提供编译期选定机器平台的静态 interface。
//!
//! ISA mechanism 属于 `arch`；firmware、启动 handoff 与设备发现属于本 module。

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
mod qemu_virt;
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
use qemu_virt as selected;

#[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("LiteOS currently has no platform implementation for this target architecture");

pub(crate) use selected::{
    BootInfo, ClaimedInterrupt, InstructionFenceError, ResetError, TlbShootdownError, arm_timer,
    claim_interrupt, complete_interrupt, complete_pending_ipi, console, debug_console_write,
    hardware_cpu_ids, initialize, initialize_devices, kernel_mmio_regions, notify_self,
    physical_memory_end, read_realtime_ns, reset_system, send_ipi, start_cpu,
    synchronize_instruction_cache, synchronize_tlb, timebase_frequency, validate_boot_info,
    verify_firmware,
};

/// whole-system firmware reset 的目标状态；具体 SBI/PSCI 编码由 platform backend 拥有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetKind {
    Shutdown,
    ColdReboot,
}

/// 向 firmware 报告的 reset 原因；不支持原因字段的 firmware 忽略该值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetReason {
    /// 用户或系统策略主动请求。
    Requested,
    /// kernel fail-stop。
    SystemFailure,
}
