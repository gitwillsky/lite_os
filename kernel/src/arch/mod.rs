#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
use riscv64 as selected;

#[cfg(not(target_arch = "riscv64"))]
compile_error!("LiteOS currently has no architecture implementation for this target");

pub(crate) use selected::{before_mmio_write, secondary_entry};

/// Local interrupt mechanism selected at compile time.
pub(crate) mod interrupt {
    pub(crate) use super::selected::interrupt::{
        LocalInterruptState, clear_software, disable_for_fail_stop, disable_for_transfer,
        disable_local, enable_scheduler_interrupts, enable_timer_source, raise_software,
        restore_local, wait_for_external_interrupt, wait_for_interrupt as wait,
    };
}

/// Kernel/user execution context selected at compile time.
pub(crate) mod context {
    pub(crate) use super::selected::{
        KernelContext, KernelResume, SignalMachineContext, SyscallCompletion, UserContext,
        switch_kernel_context,
    };
}

/// CPU-local startup and identity mechanism selected at compile time.
pub(crate) mod cpu {
    pub(crate) use super::selected::{
        StartupCpu, current_logical_id, entry_identity, initialize_local_execution,
        initialize_startup, install_boot_cpu,
    };
}

/// Architecture monotonic counter selected at compile time.
pub(crate) mod time {
    pub(crate) use super::selected::time_counter as counter;
}

/// MMU mechanism selected at compile time.
pub(crate) mod mmu {
    pub(crate) use super::selected::{
        AddressSpaceToken, ArchitecturePageTable, ArchitecturePageTableEntry, PAGE_SIZE,
        PagePermissions, PageTableError, SIGNAL_TRAMPOLINE_ADDRESS, TRAMPOLINE_ADDRESS,
        TRAP_CONTEXT_ADDRESS, TablePage, USER_ADDRESS_END, activate_address_space as activate,
        canonicalize_virtual_address, flush_local_tlb as flush_local,
        flush_local_tlb_range as flush_local_range, normalize_physical_address,
        normalize_physical_page, normalize_virtual_page,
    };
}

/// Trap entry, decoding and return mechanism selected at compile time.
pub(crate) mod trap {
    pub(crate) use super::selected::{
        TrapEvent, UserTrapEntry, install_kernel_entry, is_floating_point_instruction_at,
        kernel_exception, return_to_user, trap_event as event, user_entry,
    };
}

/// Instruction-write publication selected at compile time.
pub(crate) mod instruction {
    pub(crate) use super::selected::publish_instruction_writes as publish_local;
}

/// User-visible architecture conventions selected at compile time.
pub(crate) mod user {
    pub(crate) use super::selected::{MACHINE_NAME, hardware_probe_value};
}
