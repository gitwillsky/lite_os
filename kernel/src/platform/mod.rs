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
    BootInfo, ClaimedInterrupt, InstructionFenceError, InterruptRetirement, MessageInterruptGroup,
    MessageInterruptRoute, TlbShootdownError, arm_timer, bind_interrupt, bind_message_interrupt,
    boot_entropy, claim_interrupt, complete_interrupt, complete_pending_ipi, console,
    device_interrupt_range, dtb_range, hardware_cpu_ids, initialize, initialize_devices,
    initrd_range, kernel_command_line, kernel_mmio_regions, physical_memory_end,
    physical_memory_range, read_realtime_ns, reset_system, send_ipi, set_interrupt_affinity,
    start_cpu, synchronize_instruction_cache, synchronize_tlb, timebase_frequency,
    unbind_interrupt, unmask_interrupt, validate_boot_info, verify_firmware,
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

/// 上层注入的已清零 DMA 存储；type erasure 只保留冷分配的寿命与物理地址，不分派架构。
pub(crate) type MessageInterruptMemory = alloc::boxed::Box<dyn crate::arch::dma::DmaFrame>;
/// 分配策略属于上层；platform 只接收可失败的连续驻留存储，不反向调用 memory。
pub(crate) type MessageInterruptAllocator = fn(usize) -> Option<MessageInterruptMemory>;
