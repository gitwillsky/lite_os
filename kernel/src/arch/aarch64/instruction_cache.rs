use core::arch::asm;

/// Clean executable writes in the actual physical range to PoU.
///
/// The generic page-table transaction records the physical pages whose executable view is being
/// published. This owner derives their unique kernel direct-map aliases, so IDC=0 never cleans a
/// user alias that differs from the address used for the data writes.
pub(crate) fn prepare_range(physical_start: usize, size: usize) {
    assert_ne!(size, 0, "instruction publication range is empty");
    let physical_end = physical_start
        .checked_add(size)
        .expect("instruction publication range overflow");
    assert!(
        physical_end <= (1usize << 38),
        "instruction publication exceeds the AArch64 direct-map window"
    );
    let (idc, _) = super::startup::cache_capabilities();

    if !idc {
        let ctr: u64;
        // SAFETY: CTR_EL0 is read-only at EL1; IDC machines need no range walk or CTR read.
        unsafe {
            asm!("mrs {value}, ctr_el0", value = out(reg) ctr, options(nomem, nostack, preserves_flags))
        };
        let line = 4usize << ((ctr >> 16) & 0xf);
        let mut physical = physical_start & !(line - 1);
        while physical < physical_end {
            let address = super::mmu::physical_to_virtual(physical);
            // SAFETY: the strict direct-map conversion proves this is the kernel alias of the
            // modified physical cache line; DC CVAU cleans that line to PoU.
            unsafe { asm!("dc cvau, {address}", address = in(reg) address, options(nostack)) };
            physical += line;
        }
    }
    // SAFETY: finish data writes/clean to PoU before publishing this PTE or migrating the task.
    // Instruction invalidation is batched by the translation transaction after all ranges are clean.
    unsafe { asm!("dsb ish", options(nostack)) };
}

/// Complete the calling PE's executable publication, including unknown user virtual aliases.
pub(crate) fn publish_local() {
    let (_, dic) = super::startup::cache_capabilities();
    // SAFETY: the transaction has completed data publication for every actual range. VIPT aliases
    // cannot be enumerated from the direct map, so invalidate this PE's entire I-cache when required.
    unsafe {
        if !dic {
            asm!("ic iallu", options(nostack));
        }
        asm!("dsb ish", "isb", options(nostack));
    }
}

/// Discard firmware instruction-cache state before a CPU joins shared execution.
pub(crate) fn initialize_local() {
    // SAFETY: startup owns this CPU and invalidates only inner-shareable instruction-cache state.
    unsafe { asm!("ic ialluis", "dsb ish", "isb", options(nostack)) };
}

/// Complete instruction-cache publication across the inner-shareable PE set.
pub(crate) fn broadcast_instruction_cache() {
    let (_, dic) = super::startup::cache_capabilities();
    // SAFETY: DIC permits fetch to observe data-cache writes without invalidation. Otherwise
    // IALLUIS is the architecture-wide inner-shareable completion mechanism.
    unsafe {
        if dic {
            asm!("dsb ish", "isb", options(nostack));
        } else {
            asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack));
        }
    }
}
