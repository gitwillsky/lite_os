//! Host MMIO fixture：有效访问只指向调用者持有的对齐 buffer，不模拟硬件时序。
#![allow(dead_code)]

macro_rules! access {
    ($read:ident, $write:ident, $width:ty) => {
        // SAFETY: fixture callers keep an aligned buffer alive across each validated bus access.
        pub(crate) unsafe fn $read(address: usize) -> $width {
            unsafe { core::ptr::read_volatile(address as *const $width) }
        }

        // SAFETY: fixture callers exclusively own the writable buffer for the duration of access.
        pub(crate) unsafe fn $write(address: usize, value: $width) {
            unsafe { core::ptr::write_volatile(address as *mut $width, value) };
        }
    };
}

access!(read_mmio_u8, write_mmio_u8, u8);
access!(read_mmio_u16, write_mmio_u16, u16);
access!(read_mmio_u32, write_mmio_u32, u32);
access!(read_mmio_u64, write_mmio_u64, u64);

pub(crate) fn before_mmio_write() {}
