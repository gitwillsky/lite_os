use core::arch::asm;

use super::{
    mmu::{AddressSpaceToken, PAGE_SIZE},
    user_context::{KERNEL_STACK_CONTEXT_OFFSET, UserContext, is_kernel_stack_user_context},
};

#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct UserTrapEntry(usize);

impl UserTrapEntry {
    pub(super) fn encoded(self) -> usize {
        self.0
    }
}

/// Return the generic Rust user-trap callback as an opaque architecture target.
pub(crate) fn user_entry() -> UserTrapEntry {
    // SAFETY: entry.rs defines this symbol as the noreturn generic user-trap callback.
    unsafe extern "C" {
        fn __liteos_user_trap() -> !;
    }
    UserTrapEntry(__liteos_user_trap as *const () as usize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "generic trap matching also names RISC-V-only timer/software interrupt events"
)]
pub(crate) enum TrapEvent {
    TimerInterrupt,
    ExternalInterrupt,
    SoftwareInterrupt,
    UnsupportedInterrupt,
    IllegalInstruction,
    Breakpoint,
    UserEnvironmentCall,
    InstructionPageFault { address: usize },
    LoadPageFault { address: usize },
    StorePageFault { address: usize },
    LoadAccessFault { address: usize },
    StoreAccessFault { address: usize },
    UnsupportedException { address: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelException {
    pub(crate) event: TrapEvent,
    pub(crate) program_counter: usize,
}

/// Decode a synchronous ESR or return the single semantic IRQ route owned by platform GICv3.
pub(crate) fn event() -> TrapEvent {
    let marker: u64;
    // SAFETY: trap.S temporarily owns SP_EL0 after saving any interrupted EL0 stack pointer. Value 1 marks
    // IRQ and leaves ICC_IAR/EOIR untouched for the platform claim owner.
    unsafe {
        asm!("mrs {value}, sp_el0", value = out(reg) marker, options(nomem, nostack, preserves_flags))
    };
    if marker == 1 {
        return TrapEvent::ExternalInterrupt;
    }

    let esr: u64;
    let address: usize;
    // SAFETY: ESR/FAR are read-only snapshots of the current EL1 exception.
    unsafe {
        asm!("mrs {value}, esr_el1", value = out(reg) esr, options(nomem, nostack, preserves_flags));
        asm!("mrs {value}, far_el1", value = out(reg) address, options(nomem, nostack, preserves_flags));
    }
    let class = (esr >> 26) & 0x3f;
    match class {
        // SVE/SME are intentionally absent from the user context ABI and HWCAP. A CPU can still
        // recognize a feature-probe instruction and report its access-trap EC instead of Unknown;
        // route both forms through forced SIGILL so libc/crypto probes can recover normally.
        0x00 | 0x19 | 0x1d => TrapEvent::IllegalInstruction,
        0x15 => TrapEvent::UserEnvironmentCall,
        0x20 | 0x21 => TrapEvent::InstructionPageFault { address },
        0x24 | 0x25 => {
            let fault_status = esr & 0x3f;
            let page_fault = matches!(fault_status, 4..=7 | 9..=11 | 13..=15);
            match (page_fault, esr & (1 << 6) != 0) {
                (true, true) => TrapEvent::StorePageFault { address },
                (true, false) => TrapEvent::LoadPageFault { address },
                (false, true) => TrapEvent::StoreAccessFault { address },
                (false, false) => TrapEvent::LoadAccessFault { address },
            }
        }
        0x3c => TrapEvent::Breakpoint,
        _ => TrapEvent::UnsupportedException { address },
    }
}

pub(crate) fn kernel_exception(event: TrapEvent) -> KernelException {
    let pc: usize;
    // SAFETY: ELR_EL1 is the current exception return PC.
    unsafe {
        asm!("mrs {value}, elr_el1", value = out(reg) pc, options(nomem, nostack, preserves_flags))
    };
    KernelException {
        event,
        program_counter: pc,
    }
}

/// Install the linked AArch64 vector table for the calling CPU.
pub(crate) fn install_kernel_entry() {
    // SAFETY: trap.S defines a complete 2-KiB-aligned sixteen-entry AArch64 vector table.
    unsafe extern "C" {
        fn __aarch64_vectors();
    }
    let address = __aarch64_vectors as *const () as usize;
    assert_eq!(address & 0x7ff, 0, "AArch64 VBAR is not 2 KiB aligned");
    // SAFETY: linked symbol is a 2-KiB-aligned table with all sixteen architectural entries.
    unsafe { asm!("msr vbar_el1, {address}", "isb", address = in(reg) address, options(nostack)) };
}

/// Switch to a user address space through the mapped trampoline and enter EL0.
pub(crate) fn return_to_user(
    context_address: usize,
    address_space: AddressSpaceToken,
    trampoline_address: usize,
) -> ! {
    // SAFETY: TTBR1 keeps the linked restore entry executable while the boot TTBR0 is active.
    unsafe extern "C" {
        fn __aarch64_restore();
    }
    assert_eq!(
        trampoline_address & 0x7ff,
        0,
        "user VBAR mapping is not aligned"
    );
    assert!(
        is_kernel_stack_user_context(context_address),
        "AArch64 user context is outside the kernel stack window"
    );
    assert_eq!(
        context_address % core::mem::align_of::<UserContext>(),
        0,
        "AArch64 user context is misaligned"
    );
    let reserved_page = context_address
        .checked_sub(KERNEL_STACK_CONTEXT_OFFSET)
        .expect("AArch64 user context offset underflow");
    assert_eq!(reserved_page & (PAGE_SIZE - 1), 0);
    assert!(
        context_address
            .checked_add(core::mem::size_of::<UserContext>())
            .is_some_and(|end| end <= reserved_page + PAGE_SIZE),
        "AArch64 user context exceeds its reserved kernel-stack page"
    );
    // SAFETY: x0/x1 own the live user context/token and x2 is its mapped vector-table VA. The
    // high-half restore entry installs TTBR0 before publishing x2 to VBAR_EL1 and never returns.
    unsafe {
        asm!(
            "br {restore}",
            restore = in(reg) __aarch64_restore as *const () as usize,
            in("x0") context_address,
            in("x1") address_space.encoded(),
            in("x2") trampoline_address,
            options(noreturn)
        )
    }
}
