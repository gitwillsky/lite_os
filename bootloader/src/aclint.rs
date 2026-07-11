use core::{arch::naked_asm, cell::UnsafeCell};

/// MTIME register.
#[repr(transparent)]
pub struct MTIME(UnsafeCell<u64>);

/// One MTIMECMP register.
#[repr(transparent)]
pub struct MTIMECMP(UnsafeCell<u64>);

/// One MSIP register.
#[repr(transparent)]
pub struct MSIP(UnsafeCell<u32>);

/// Machine-level Timer Device (MTIMER).
#[repr(transparent)]
pub struct MTIMER([MTIMECMP; 4095]);

/// Machine-level Software Interrupt Device (MSWI).
#[repr(transparent)]
pub struct MSWI([MSIP; 4095]);

/// Sifive CLINT device.
#[repr(C)]
pub struct SifiveClint {
    mswi: MSWI,
    reserve: u32,
    mtimer: MTIMER,
    _mtime: MTIME,
}

impl SifiveClint {
    const MTIMER_OFFSET: usize = size_of::<MSWI>() + size_of::<u32>();

    #[inline]
    pub fn write_mtimecmp(&self, hart_idx: usize, val: u64) {
        unsafe { self.mtimer.0[hart_idx].0.get().write_volatile(val) }
    }

    #[inline]
    pub fn set_msip(&self, hart_idx: usize) {
        unsafe { self.mswi.0[hart_idx].0.get().write_volatile(1) }
    }

    #[inline]
    pub fn clear_msip(&self, hart_idx: usize) {
        unsafe { self.mswi.0[hart_idx].0.get().write_volatile(0) }
    }
}

impl SifiveClint {
    #[unsafe(naked)]
    pub unsafe extern "C" fn write_mtimecmp_naked(&self, hart_idx: usize, val: u64) {
        naked_asm!(
            "   slli a1, a1, 3
                    add  a0, a0, a1

                    li   a1, {offset}
                    add  a0, a0, a1

                    sd   a2, (a0)
                    ret
                ",
            offset = const Self::MTIMER_OFFSET,
        )
    }

    #[unsafe(naked)]
    pub unsafe extern "C" fn clear_msip_naked(&self, hart_idx: usize) {
        naked_asm!(
            "   slli a1, a1, 2
                    add  a0, a0, a1
                    sw   zero, (a0)
                    ret
                ",
        )
    }
}
