//! @description 提供编译期选定机器平台的静态 interface。
//!
//! ISA mechanism 属于 `arch`；firmware、启动 handoff 与设备发现属于本 module。

#[cfg(target_arch = "riscv64")]
mod qemu_virt;
#[cfg(target_arch = "riscv64")]
use qemu_virt as selected;

#[cfg(not(target_arch = "riscv64"))]
compile_error!("LiteOS currently has no platform implementation for this target architecture");

pub(crate) use selected::{
    BootInfo, ResetError, TlbShootdownError, arm_timer, console, debug_console_write,
    handle_external_interrupt, hardware_cpu_ids, initialize, initialize_devices,
    kernel_mmio_regions, physical_memory_end, read_realtime_ns, reset_system, send_ipi, start_cpu,
    synchronize_tlb, timebase_frequency, validate_boot_info, verify_firmware,
};
