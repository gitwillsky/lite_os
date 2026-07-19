use core::arch::asm;

/// Publish executable writes covering one physical-address range.
///
/// The generic page-table transaction records the physical pages whose executable view is being
/// published. This owner derives their unique kernel direct-map aliases, so IDC=0 never cleans a
/// user alias that differs from the address used for the data writes.
pub(crate) fn publish_range(physical_start: usize, size: usize) {
    assert_ne!(size, 0, "instruction publication range is empty");
    let physical_end = physical_start
        .checked_add(size)
        .expect("instruction publication range overflow");
    assert!(
        physical_end <= (1usize << 38),
        "instruction publication exceeds the AArch64 direct-map window"
    );
    let (idc, dic) = super::startup::cache_capabilities();
    if idc && dic {
        // SAFETY: IDC/DIC guarantee coherency to PoU without explicit line maintenance; the
        // barriers publish preceding writes and synchronize subsequent instruction fetch.
        unsafe { asm!("dsb ish", "isb", options(nostack)) };
        return;
    }

    let ctr: u64;
    // SAFETY: CTR_EL0 is read-only at EL1.
    unsafe {
        asm!("mrs {value}, ctr_el0", value = out(reg) ctr, options(nomem, nostack, preserves_flags))
    };
    if !idc {
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
    // SAFETY: data clean, when required, must complete before invalidating instruction lines.
    unsafe { asm!("dsb ish", options(nostack)) };
    if !dic {
        let line = 4usize << (ctr & 0xf);
        let mut physical = physical_start & !(line - 1);
        while physical < physical_end {
            let address = super::mmu::physical_to_virtual(physical);
            // SAFETY: address is the bounded direct-map alias of this physical line; IC IVAU
            // invalidates that exact line to PoU for the calling PE.
            unsafe { asm!("ic ivau, {address}", address = in(reg) address, options(nostack)) };
            physical += line;
        }
    }
    // SAFETY: completion and context synchronization are required before executable use.
    unsafe { asm!("dsb ish", "isb", options(nostack)) };
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
