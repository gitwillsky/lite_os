use core::arch::asm;

/// Read one byte from a caller-validated MMIO address with an exact base-register access.
///
/// # Safety
///
/// `address` must name a readable byte-wide device register in a permanently mapped MMIO range.
// SAFETY: callers must prove the address is a readable byte-wide device register.
#[inline(always)]
pub(crate) unsafe fn read_mmio_u8(address: usize) -> u8 {
    let value: u32;
    // SAFETY: the caller owns address validity. Keeping the address in a dedicated base register
    // prevents LLVM from selecting a post-indexed load whose HVF exit has no valid syndrome.
    unsafe {
        asm!(
            "ldrb {value:w}, [{address}]",
            value = out(reg) value,
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
    value as u8
}

/// Write one byte to a caller-validated MMIO address with an exact base-register access.
///
/// # Safety
///
/// `address` must name a writable byte-wide device register in a permanently mapped MMIO range.
// SAFETY: callers must prove the address is a writable byte-wide device register.
#[inline(always)]
pub(crate) unsafe fn write_mmio_u8(address: usize, value: u8) {
    // SAFETY: the caller owns address validity; one base-only STRB cannot be merged or indexed.
    unsafe {
        asm!(
            "strb {value:w}, [{address}]",
            value = in(reg) u32::from(value),
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
}

/// Read one 32-bit word from a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a readable, 32-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a readable aligned 32-bit device register.
#[inline(always)]
pub(crate) unsafe fn read_mmio_u32(address: usize) -> u32 {
    let value: u32;
    // SAFETY: the caller owns range/alignment validity; the template fixes one exact LDR.
    unsafe {
        asm!(
            "ldr {value:w}, [{address}]",
            value = out(reg) value,
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
    value
}

/// Write one 32-bit word to a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a writable, 32-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a writable aligned 32-bit device register.
#[inline(always)]
pub(crate) unsafe fn write_mmio_u32(address: usize, value: u32) {
    // SAFETY: the caller owns range/alignment validity; the template fixes one exact STR.
    unsafe {
        asm!(
            "str {value:w}, [{address}]",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
}

/// Read one 64-bit word from a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a readable, 64-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a readable aligned 64-bit device register.
#[inline(always)]
pub(crate) unsafe fn read_mmio_u64(address: usize) -> u64 {
    let value: u64;
    // SAFETY: the caller owns range/alignment validity; the template fixes one exact LDR.
    unsafe {
        asm!(
            "ldr {value}, [{address}]",
            value = out(reg) value,
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
    value
}

/// Write one 64-bit word to a caller-validated, aligned MMIO address.
///
/// # Safety
///
/// `address` must name a writable, 64-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a writable aligned 64-bit device register.
#[inline(always)]
pub(crate) unsafe fn write_mmio_u64(address: usize, value: u64) {
    // SAFETY: the caller owns range/alignment validity; the template fixes one exact STR.
    unsafe {
        asm!(
            "str {value}, [{address}]",
            value = in(reg) value,
            address = in(reg) address,
            options(nostack, preserves_flags)
        )
    };
}

/// Order all earlier normal-memory writes before a following MMIO output.
///
/// OWNER: this module owns the AArch64 normal-memory-to-device ordering edge. Without it, a
/// VirtIO doorbell may become visible before the descriptors it publishes.
#[inline(always)]
pub(crate) fn before_mmio_write() {
    // SAFETY: DMB changes only architectural ordering. The memory clobber implied by omitting
    // `nomem` also prevents compiler movement across the publication edge.
    unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) }
}
