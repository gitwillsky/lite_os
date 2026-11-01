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

/// Read one 64-bit doubleword from a caller-validated, 8-byte-aligned MMIO address.
///
/// # Safety
///
/// `address` must name a readable, 64-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a readable aligned 64-bit device register.
#[inline(always)]
#[allow(
    dead_code,
    reason = "complete MMIO width set; only AArch64 GICv3 uses 64-bit registers"
)]
pub(crate) unsafe fn read_mmio_u64(address: usize) -> u64 {
    // SAFETY: the caller owns range and alignment validity.
    unsafe { core::ptr::read_volatile(address as *const u64) }
}

/// Write one 64-bit doubleword to a caller-validated, 8-byte-aligned MMIO address.
///
/// # Safety
///
/// `address` must name a writable, 64-bit-aligned device register in a permanent MMIO mapping.
// SAFETY: callers must prove the address is a writable aligned 64-bit device register.
#[inline(always)]
#[allow(
    dead_code,
    reason = "complete MMIO width set; only AArch64 GICv3 uses 64-bit registers"
)]
pub(crate) unsafe fn write_mmio_u64(address: usize, value: u64) {
    // SAFETY: the caller owns range and alignment validity.
    unsafe { core::ptr::write_volatile(address as *mut u64, value) };
}

/// 让此前的普通内存写入先于随后的 MMIO 写入可见（DMA 描述符发布后再敲 doorbell）。
///
/// OWNER: 本模块拥有普通内存到设备的写入次序边；缺失时设备可能先看到 doorbell、后看到描述符。
#[inline(always)]
pub(crate) fn before_mmio_write() {
    // SAFETY: 屏障只改变架构内存次序；省略 `nomem` 同时阻止编译器跨越发布边移动内存写入。
    unsafe { asm!("fence w, o", options(nostack, preserves_flags)) }
}
