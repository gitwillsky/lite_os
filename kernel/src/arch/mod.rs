mod address_space_id;

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

/// 无法由架构惰性状态消化、必须作为异常报告的用户非法指令。
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

    /// 异常报告中的用户指令地址。
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

pub(crate) use selected::secondary_entry;

/// Local interrupt mechanism selected at compile time.
pub(crate) mod interrupt {
    pub(crate) use super::selected::interrupt::raise_software;
    pub(crate) use super::selected::interrupt::{
        LocalInterruptState, clear_software, disable_for_fail_stop, disable_for_transfer,
        disable_local, disable_timer_source, enable_scheduler_interrupts, enable_timer_source,
        restore_local, wait_for_interrupt as wait, wait_with_local_irq_masked,
    };
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::interrupt::{SOFTWARE_SGI, send_software_sgi};
}

/// Frame-pointer unwinding mechanism selected at compile time.
pub(crate) mod backtrace {
    pub(crate) use super::selected::backtrace::{frame_pointer, read_frame, stack_pointer};
    pub(crate) use super::selected::startup_stack_bounds;
}

/// Kernel/user execution context selected at compile time.
pub(crate) mod context {
    pub(crate) use super::selected::{
        KERNEL_STACK_CONTEXT_RESERVE, KernelContext, KernelResume, USER_CONTEXT_PLACEMENT,
        UserContext, inspect_illegal_instruction, switch_kernel_context,
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
    pub(crate) use super::selected::{
        AddressSpaceKind, AddressSpaceToken, ArchitecturePageTable, ArchitecturePageTableEntry,
        KERNEL_STACK_REGION_START, KERNEL_STACK_REGION_TOP, KernelTrapToken, PAGE_SIZE,
        PagePermissions, PageTableError, THREAD_TRAP_CONTEXT_RANGE, TRAMPOLINE_ADDRESS,
        TRAP_CONTEXT_ADDRESS, TablePage, USER_ADDRESS_END, canonicalize_virtual_address,
        flush_local_tlb as flush_local, flush_local_tlb_range as flush_local_range,
        initialize_asid_allocator, initialize_kernel_mmio, normalize_physical_address,
        normalize_physical_page, normalize_virtual_page, physical_range_to_virtual,
        physical_to_virtual, virtual_to_physical,
    };
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::{
        MEMORY_ATTRIBUTES, TRANSLATION_BITS, acknowledge_broadcast_tlb, broadcast_tlb,
    };
}

/// Trap entry, decoding and return mechanism selected at compile time.
pub(crate) mod trap {
    pub(crate) use super::selected::{
        TrapEvent, UserFault, UserTrapEntry, install_kernel_entry, kernel_exception,
        return_to_user, trap_event as event, user_entry,
    };
}

/// Instruction-write publication selected at compile time.
pub(crate) mod instruction {
    #[cfg(target_arch = "aarch64")]
    pub(crate) use super::selected::broadcast_instruction_cache;
    pub(crate) use super::selected::publish_instruction_range as publish_range;
}

/// Device translation register/table encoding and allocation seam.
pub(crate) mod dma;

/// ELF machine identifier accepted by the native loader.
#[cfg(target_arch = "aarch64")]
pub(crate) const ELF_MACHINE: u16 = 183;
#[cfg(target_arch = "riscv64")]
pub(crate) const ELF_MACHINE: u16 = 243;
