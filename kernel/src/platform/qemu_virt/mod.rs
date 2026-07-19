//! @description QEMU `virt` platform 的编译期静态 façade。

#[cfg(target_arch = "riscv64")]
#[macro_use]
mod riscv64;

#[cfg(target_arch = "aarch64")]
#[macro_use]
mod aarch64;

#[cfg(target_arch = "aarch64")]
use aarch64 as selected;
#[cfg(target_arch = "riscv64")]
use riscv64 as selected;

/// @description GIC/PLIC claim 后交给 generic trap domain 的语义中断与 opaque completion token。
// RISC-V external claim 只产生 Device；其 local timer/software traps 不经过 controller seam。
// 缺少该 target-owned lint projection 时，保留的语义 union 会被 `-D warnings` 误判为 dead code。
#[cfg_attr(
    target_arch = "riscv64",
    allow(dead_code, reason = "RISC-V controller only constructs Device")
)]
pub(crate) enum ClaimedInterrupt {
    Timer(u32),
    Device(u32),
    Software(u32),
    Spurious,
}

impl ClaimedInterrupt {
    fn completion_token(&self) -> Option<u32> {
        match self {
            Self::Timer(token) | Self::Device(token) | Self::Software(token) => Some(*token),
            Self::Spurious => None,
        }
    }
}

pub(crate) use selected::{
    BootInfo, InstructionFenceError, ResetError, TlbShootdownError, arm_timer, claim_interrupt,
    complete_interrupt, console, debug_console_write, hardware_cpu_ids, initialize,
    initialize_devices, kernel_mmio_regions, notify_self, physical_memory_end, read_realtime_ns,
    reset_system, send_ipi, start_cpu, synchronize_instruction_cache, synchronize_tlb,
    timebase_frequency, validate_boot_info, verify_firmware,
};
