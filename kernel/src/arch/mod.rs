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

/// A genuine user illegal instruction that must become synchronous `SIGILL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IllegalInstructionFault {
    address: usize,
}

/// Architecture-owned first-stage classification of a user illegal instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IllegalInstructionProbe {
    /// No architecture state transition can consume the exception.
    Fault(IllegalInstructionFault),
    // AArch64 eagerly enables FP/ASIMD and therefore never asks for instruction decoding.
    // Without this target-owned lint projection, `-D warnings` rejects the shared semantic result.
    #[cfg_attr(
        target_arch = "aarch64",
        allow(dead_code, reason = "AArch64 never requests lazy instruction decode")
    )]
    Decode { address: usize },
}

/// Verified lazy architecture-state transition to commit in a short context transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IllegalInstructionRetry {
    address: usize,
}

#[cfg_attr(
    target_arch = "aarch64",
    allow(dead_code, reason = "AArch64 never commits lazy instruction retry")
)]
impl IllegalInstructionRetry {
    /// Record the instruction address whose lazy state transition was verified.
    pub(crate) const fn new(address: usize) -> Self {
        Self { address }
    }

    /// Return the verified instruction address for commit-time validation.
    pub(crate) const fn address(self) -> usize {
        self.address
    }
}

impl IllegalInstructionFault {
    /// Construct a fault at the architecture-owned user program counter.
    pub(crate) const fn new(address: usize) -> Self {
        Self { address }
    }

    /// Return the user instruction address reported to `siginfo_t`.
    pub(crate) const fn address(self) -> usize {
        self.address
    }
}

/// Compile-time placement of each Thread's architecture user context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UserContextPlacement {
    /// The context lives at a fixed offset in the Thread's kernel-stack reservation.
    #[cfg_attr(
        target_arch = "riscv64",
        allow(dead_code, reason = "RISC-V uses an address-space trap context")
    )]
    KernelStack { offset: usize },
    /// The context lives in the process supervisor address-space mapping.
    #[cfg_attr(
        target_arch = "aarch64",
        allow(dead_code, reason = "AArch64 uses a kernel-stack user context")
    )]
    AddressSpace,
}

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
        disable_local, disable_timer_source, enable_scheduler_interrupts, enable_timer_source,
        restore_local, wait_for_external_interrupt, wait_for_interrupt as wait,
        wait_with_local_irq_masked,
    };
}

/// Kernel/user execution context selected at compile time.
pub(crate) mod context {
    pub(crate) use super::selected::{
        KERNEL_STACK_CONTEXT_RESERVE, KernelContext, KernelResume, MIN_SIGNAL_STACK_SIZE,
        SIGNAL_FRAME_SIZE, SignalFrame, SignalStack, SyscallCompletion, USER_CONTEXT_PLACEMENT,
        UserContext, inspect_illegal_instruction, reset_live_floating_point, switch_kernel_context,
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
        TrapEvent, UserTrapEntry, install_kernel_entry, kernel_exception, return_to_user,
        trap_event as event, user_entry,
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
        ELF_HWCAP, ELF_MACHINE, MACHINE_NAME, decode_private_syscall, valid_elf_flags,
    };
}
