use core::arch::global_asm;

pub(crate) mod backtrace;
mod instruction_cache;
pub(crate) mod interrupt;
mod kernel_context;
mod mmu;
mod page_table;
mod pte;
mod start;
mod startup;
pub(crate) use startup::startup_stack_bounds;
mod time;
mod trap;
mod user_context;
mod va39;

pub(crate) use instruction_cache::broadcast_instruction_cache;
pub(crate) use instruction_cache::{
    prepare_range as prepare_instruction_range, publish_local as publish_instruction_local,
};
pub(crate) use kernel_context::{KernelContext, KernelResume, switch_kernel_context};
pub(crate) use mmu::{
    AddressSpaceToken, KERNEL_STACK_REGION_START, KERNEL_STACK_REGION_TOP, KernelTrapToken,
    MEMORY_ATTRIBUTES, PAGE_SIZE, THREAD_TRAP_CONTEXT_RANGE, TRAMPOLINE_ADDRESS, TRANSLATION_BITS,
    TRAP_CONTEXT_ADDRESS, USER_ADDRESS_END, acknowledge_broadcast_tlb, broadcast_tlb,
    canonicalize_virtual_address, flush_local as flush_local_tlb,
    flush_local_range as flush_local_tlb_range, initialize_asid_allocator, initialize_kernel_mmio,
    normalize_physical_address, normalize_physical_page, normalize_virtual_page,
    physical_range_to_virtual, physical_to_virtual, virtual_to_physical,
};
pub(crate) use page_table::{
    AddressSpaceKind, PageTable as ArchitecturePageTable,
    PageTableEntry as ArchitecturePageTableEntry, PageTableError, TablePage,
};
pub(crate) use pte::PagePermissions;
pub(crate) use start::entry_address as secondary_entry;
pub(crate) use startup::{
    StartupCpu, current_logical_id, entry_identity, initialize as initialize_startup,
    initialize_local_execution, install_boot_logical_id as install_boot_cpu,
};
pub(crate) use time::counter as time_counter;
pub(crate) use time::{counter_frequency, program_virtual_timer};
pub(crate) use trap::{
    TrapEvent, UserFault, UserTrapEntry, event as trap_event, install_kernel_entry,
    kernel_exception, return_to_user, user_entry,
};
pub(crate) use user_context::{
    KERNEL_STACK_CONTEXT_RESERVE, USER_CONTEXT_PLACEMENT, UserContext, inspect_illegal_instruction,
};

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));
