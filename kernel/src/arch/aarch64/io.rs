use core::arch::asm;

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

/// 让此前的普通内存写入先于随后的 MMIO 写入可见（DMA 描述符发布后再敲 doorbell）。
///
/// OWNER: 本模块拥有普通内存到设备的写入次序边；缺失时设备可能先看到 doorbell、后看到描述符。
#[inline(always)]
pub(crate) fn before_mmio_write() {
    // SAFETY: 屏障只改变架构内存次序；省略 `nomem` 同时阻止编译器跨越发布边移动内存写入。
    unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) }
}
