use core::arch::asm;

/// @description Order all earlier normal-memory writes before a following MMIO output.
///
/// @return No value; later MMIO writes cannot become visible before earlier memory writes.
/// @errors No recoverable error.
///
/// OWNER: `arch::io` owns the target-specific normal-memory-to-device ordering mechanism.
/// Without this `w -> o` edge, a device doorbell can pass the shared-memory state it publishes.
#[inline(always)]
pub(crate) fn before_mmio_write() {
    // SAFETY: `fence` changes only architectural ordering.  Omitting `nomem` also makes this a
    // compiler barrier for the shared-memory writes that must precede the following MMIO access.
    unsafe { asm!("fence w, o", options(nostack, preserves_flags)) }
}
