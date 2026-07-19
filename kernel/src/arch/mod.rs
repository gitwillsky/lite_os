#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as selected;

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
use riscv64 as selected;

#[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
compile_error!("LiteOS currently has no architecture implementation for this target");

pub(crate) use selected::{
    before_mmio_write, read_mmio_u8, read_mmio_u32, secondary_entry, write_mmio_u8, write_mmio_u32,
};
#[cfg(target_arch = "aarch64")]
pub(crate) use selected::{read_mmio_u64, write_mmio_u64};

/// Local interrupt mechanism selected at compile time.
pub(crate) mod interrupt {
    #[cfg(target_arch = "riscv64")]
    pub(crate) use super::selected::interrupt::raise_software;
    pub(crate) use super::selected::interrupt::{
        LocalInterruptState, clear_software, disable_for_fail_stop, disable_for_transfer,
        disable_local, enable_scheduler_interrupts, enable_timer_source, restore_local,
        wait_for_external_interrupt, wait_for_interrupt as wait, wait_with_local_irq_masked,
    };
}

/// Kernel/user execution context selected at compile time.
pub(crate) mod context {
    pub(crate) use super::selected::{
        KERNEL_STACK_CONTEXT_RESERVE, KernelContext, KernelResume, MIN_SIGNAL_STACK_SIZE,
        SIGNAL_FRAME_SIZE, SignalFrame, SignalStack, SyscallCompletion, UserContext,
        is_kernel_stack_user_context, kernel_stack_user_context, reset_live_floating_point,
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
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::{counter_frequency, program_virtual_timer};
}

/// MMU mechanism selected at compile time.
pub(crate) mod mmu {
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::broadcast_tlb;
    pub(crate) use super::selected::{
        AddressSpaceKind, AddressSpaceToken, ArchitecturePageTable, ArchitecturePageTableEntry,
        KERNEL_STACK_REGION_START, KERNEL_STACK_REGION_TOP, KernelTrapToken, PAGE_SIZE,
        PagePermissions, PageTableError, SIGNAL_TRAMPOLINE_ADDRESS, TRAMPOLINE_ADDRESS,
        TRAP_CONTEXT_ADDRESS, TablePage, USER_ADDRESS_END, USER_STACK_TOP,
        canonicalize_virtual_address, flush_local_tlb as flush_local,
        flush_local_tlb_range as flush_local_range, normalize_physical_address,
        normalize_physical_page, normalize_virtual_page, physical_to_virtual, virtual_to_physical,
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
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::broadcast_instruction_cache;
    pub(crate) use super::selected::publish_instruction_range as publish_range;
}

/// User-visible architecture conventions selected at compile time.
pub(crate) mod user {
    pub(crate) use super::selected::{
        ELF_HWCAP, ELF_MACHINE, MACHINE_NAME, SUPPORTS_RISCV_HWPROBE, hardware_probe_value,
        valid_elf_flags,
    };
}
