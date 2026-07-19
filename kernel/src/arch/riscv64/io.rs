use core::arch::asm;

/// Read one byte from a caller-validated MMIO address.
///
/// # Safety
///
/// `address` must name a readable byte-wide device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a readable byte-wide device register.
#[inline(always)]
pub(crate) unsafe fn read_mmio_u8(address: usize) -> u8 {
    // SAFETY: the caller owns range validity; volatile preserves the device transaction.
    unsafe { core::ptr::read_volatile(address as *const u8) }
}

/// Write one byte to a caller-validated MMIO address.
///
/// # Safety
///
/// `address` must name a writable byte-wide device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a writable byte-wide device register.
#[inline(always)]
pub(crate) unsafe fn write_mmio_u8(address: usize, value: u8) {
    // SAFETY: the caller owns range validity; volatile preserves the device transaction.
    unsafe { core::ptr::write_volatile(address as *mut u8, value) };
}

/// Read one 32-bit word from a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a readable, aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a readable aligned 32-bit device register.
#[inline(always)]
pub(crate) unsafe fn read_mmio_u32(address: usize) -> u32 {
    // SAFETY: the caller owns range and alignment validity.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// Write one 32-bit word to a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a writable, aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a writable aligned 32-bit device register.
#[inline(always)]
pub(crate) unsafe fn write_mmio_u32(address: usize, value: u32) {
    // SAFETY: the caller owns range and alignment validity.
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
}

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
