/// Read the AArch64 generic virtual monotonic counter.
#[inline(always)]
pub(crate) fn counter() -> u64 {
    let value: u64;
    // SAFETY: CNTVCT_EL0 is a read-only per-system counter made accessible to EL1 by architecture.
    unsafe {
        core::arch::asm!("mrs {value}, cntvct_el0", value = out(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// Return the immutable generic-counter frequency reported by the CPU.
#[inline(always)]
pub(crate) fn counter_frequency() -> u64 {
    let value: u64;
    // SAFETY: CNTFRQ_EL0 is read-only at EL1.
    unsafe {
        core::arch::asm!("mrs {value}, cntfrq_el0", value = out(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// Program the calling CPU's generic virtual timer deadline.
pub(crate) fn program_virtual_timer(deadline: u64) {
    // SAFETY: each CPU exclusively owns CNTV_CVAL. Writing the future CVAL deasserts an expired
    // level before the GIC owner completes the active PPI; interrupt source masking stays in the
    // architecture interrupt owner.
    unsafe {
        core::arch::asm!(
            "msr cntv_cval_el0, {deadline}",
            "isb",
            deadline = in(reg) deadline,
            options(nomem, nostack, preserves_flags)
        )
    };
}
