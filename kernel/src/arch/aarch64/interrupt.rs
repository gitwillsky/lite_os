//! AArch64 CPU-local interrupt masking and generic virtual-timer source controls.

use core::arch::asm;

/// Opaque snapshot of the calling CPU's IRQ mask bit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalInterruptState {
    masked: bool,
}

/// Mask IRQ exceptions locally and return the previous state.
#[inline(always)]
pub(crate) fn disable_local() -> LocalInterruptState {
    let daif: u64;
    // SAFETY: DAIF is CPU-local; this changes only the IRQ mask after capturing it.
    unsafe {
        asm!("mrs {value}, daif", value = out(reg) daif, options(nomem, nostack, preserves_flags));
        asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
    LocalInterruptState {
        masked: daif & (1 << 7) != 0,
    }
}

/// Restore a local IRQ state obtained on the same CPU.
// SAFETY: caller must return the opaque state to the CPU that captured it.
pub(crate) unsafe fn restore_local(state: LocalInterruptState) {
    if !state.masked {
        // SAFETY: caller proves CPU identity and only the local IRQ mask is changed.
        unsafe { asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
    }
}

/// Enable IRQ delivery after VBAR and the platform controller are initialized.
// SAFETY: caller must establish trap-vector and GIC initialization ordering.
pub(crate) unsafe fn enable_scheduler_interrupts() {
    // SAFETY: caller established the required local initialization ordering.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags)) };
}

/// Wait for one IRQ while preserving the caller's local IRQ mask.
pub(crate) fn wait_for_external_interrupt() {
    let state = disable_local();
    // SAFETY: VBAR/GIC initialization is a caller invariant of bootstrap device waits. Assembly
    // temporarily unmasks IRQ delivery and exposes exact WFI/resume labels; kernel IRQ entry
    // advances ELR to resume when an IRQ lands in the enable-to-WFI window, so an acknowledged
    // one-shot device edge cannot return into an unbounded second sleep.
    wait_once_with_local_irq_masked();
    // SAFETY: no context switch occurs in the bootstrap wait.
    unsafe { restore_local(state) };
}

/// Wait for one IRQ while the caller keeps ownership of an already-masked local IRQ state.
///
/// The linked WFI/resume identity closes the enable-to-WFI lost-edge window. Returning with IRQ
/// masked lets the caller re-check scheduler state before its guard restores the prior mask.
///
/// # Panics
///
/// This function does not validate DAIF.I; callers must hold a local IRQ guard on this CPU.
pub(crate) fn wait_with_local_irq_masked() {
    wait_once_with_local_irq_masked();
}

#[inline(always)]
fn wait_once_with_local_irq_masked() {
    // SAFETY: this declaration is implemented by trap.S and preserves the Rust ABI while
    // temporarily changing only the calling CPU's IRQ mask.
    unsafe extern "C" {
        fn __wait_with_local_irq_masked();
    }
    // SAFETY: callers establish that local IRQ is masked and no context switch occurs inside the
    // assembly seam. It temporarily unmasks IRQ and always masks it again before returning.
    unsafe { __wait_with_local_irq_masked() };
}

/// Enable the calling CPU's generic virtual timer source.
// SAFETY: caller must program the first CVAL deadline before unmasking IRQ delivery.
pub(crate) unsafe fn enable_timer_source() {
    // ENABLE=1, IMASK=0. ISTATUS is read-only.
    let control = 1u64;
    // SAFETY: caller owns local timer initialization and the register is CPU-local.
    unsafe {
        asm!("msr cntv_ctl_el0, {value}", "isb", value = in(reg) control, options(nomem, nostack, preserves_flags))
    };
}

/// Software interrupt completion belongs to the platform claim owner.
pub(crate) fn clear_software() {
    panic!("AArch64 software interrupt completion must be routed through platform GICv3")
}

/// Permanently mask local IRQ delivery in a fail-stop path.
pub(crate) fn disable_for_fail_stop() {
    // SAFETY: fail-stop never restores delivery.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack, preserves_flags)) };
}

/// Permanently mask local IRQ delivery before a noreturn transfer.
pub(crate) fn disable_for_transfer() {
    disable_for_fail_stop();
}

/// Wait for the next architectural event.
#[inline(always)]
pub(crate) fn wait_for_interrupt() {
    // SAFETY: WFI only changes execution state until an event is observed.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}
