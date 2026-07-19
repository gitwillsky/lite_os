use core::arch::global_asm;

mod fp_instruction;
mod instruction_cache;
pub(crate) mod interrupt;
mod io;
mod kernel_context;
mod mmu;
mod page_table;
mod pte;
mod signal_frame;
mod start;
mod startup;
mod sv39;
mod time;
mod trap;
mod user;
mod user_context;

pub(crate) use fp_instruction::is_floating_point_instruction_at;
pub(crate) use instruction_cache::publish_range as publish_instruction_range;
pub(crate) use io::{
    before_mmio_write, read_mmio_u8, read_mmio_u32, write_mmio_u8, write_mmio_u32,
};
pub(crate) use kernel_context::{KernelContext, KernelResume, switch_kernel_context};
pub(crate) use mmu::{
    AddressSpaceToken, KERNEL_STACK_REGION_START, KERNEL_STACK_REGION_TOP, KernelTrapToken,
    PAGE_SIZE, SIGNAL_TRAMPOLINE_ADDRESS, TRAMPOLINE_ADDRESS, TRAP_CONTEXT_ADDRESS,
    USER_ADDRESS_END, USER_STACK_TOP, canonicalize_virtual_address, flush_local as flush_local_tlb,
    flush_local_range as flush_local_tlb_range, normalize_physical_address,
    normalize_physical_page, normalize_virtual_page, physical_to_virtual, virtual_to_physical,
};
pub(crate) use page_table::{
    AddressSpaceKind, PageTable as ArchitecturePageTable,
    PageTableEntry as ArchitecturePageTableEntry, PageTableError, TablePage,
};
pub(crate) use pte::PagePermissions;
pub(crate) use signal_frame::{MIN_SIGNAL_STACK_SIZE, SIGNAL_FRAME_SIZE, SignalFrame, SignalStack};
pub(crate) use start::entry_address as secondary_entry;
pub(crate) use startup::{
    StartupCpu, current_logical_id, entry_identity, initialize as initialize_startup,
    initialize_local_execution, install_boot_logical_id as install_boot_cpu,
};
pub(crate) use time::counter as time_counter;
pub(crate) use trap::{
    TrapEvent, UserTrapEntry, event as trap_event, install_kernel_entry, kernel_exception,
    return_to_user, user_entry,
};
pub(crate) use user::{
    ELF_HWCAP, ELF_MACHINE, MACHINE_NAME, SUPPORTS_RISCV_HWPROBE, SyscallCompletion,
    hardware_probe_value, valid_elf_flags,
};
pub(crate) use user_context::{
    KERNEL_STACK_CONTEXT_RESERVE, UserContext, is_kernel_stack_user_context,
    kernel_stack_user_context,
};

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));

/// @description RISC-V exec 的 live FP state 已随 UserContext 替换，无额外 CPU-local image。
/// @return 无返回值。
pub(crate) fn reset_live_floating_point() {}
