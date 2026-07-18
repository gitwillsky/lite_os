use core::arch::global_asm;

pub(crate) mod interrupt;
mod io;
mod kernel_context;
mod mmu;
mod page_table;
mod pte;
mod start;
mod startup;
mod sv39;
mod time;
mod trap;
mod user;
mod user_context;

pub(crate) use io::before_mmio_write;
pub(crate) use kernel_context::{KernelContext, KernelResume, switch_kernel_context};
pub(crate) use mmu::{
    AddressSpaceToken, PAGE_SIZE, SIGNAL_TRAMPOLINE_ADDRESS, TRAMPOLINE_ADDRESS,
    TRAP_CONTEXT_ADDRESS, USER_ADDRESS_END, activate as activate_address_space,
    canonicalize_virtual_address, flush_local as flush_local_tlb, normalize_physical_address,
    normalize_physical_page, normalize_virtual_page,
};
pub(crate) use page_table::{
    PageTable as ArchitecturePageTable, PageTableEntry as ArchitecturePageTableEntry,
    PageTableError, TablePage,
};
pub(crate) use pte::PagePermissions;
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
    MACHINE_NAME, SignalMachineContext, SyscallCompletion, hardware_probe_value,
};
pub(crate) use user_context::UserContext;

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));
