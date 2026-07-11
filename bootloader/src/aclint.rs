use core::{arch::naked_asm, cell::UnsafeCell};

/// Mtime register.
#[repr(transparent)]
pub(crate) struct Mtime(UnsafeCell<u64>);

/// One Mtimecmp register.
#[repr(transparent)]
pub(crate) struct Mtimecmp(UnsafeCell<u64>);

/// One Msip register.
#[repr(transparent)]
pub(crate) struct Msip(UnsafeCell<u32>);

/// Machine-level Timer Device (Mtimer).
#[repr(transparent)]
pub(crate) struct Mtimer([Mtimecmp; 4095]);

/// Machine-level Software Interrupt Device (Mswi).
#[repr(transparent)]
pub(crate) struct Mswi([Msip; 4095]);

/// Sifive CLINT device.
#[repr(C)]
pub(crate) struct SifiveClint {
    mswi: Mswi,
    reserve: u32,
    mtimer: Mtimer,
    _mtime: Mtime,
}

impl SifiveClint {
    const MTIMER_OFFSET: usize = size_of::<Mswi>() + size_of::<u32>();

    #[inline]
    pub(crate) fn write_mtimecmp(&self, hart_idx: usize, val: u64) {
        // SAFETY: caller bounds hart_idx to the DTB hart set represented by this CLINT mapping;
        // register cells are MMIO and therefore require a volatile write.
        unsafe { self.mtimer.0[hart_idx].0.get().write_volatile(val) }
    }

    #[inline]
    pub(crate) fn set_msip(&self, hart_idx: usize) {
        // SAFETY: caller bounds hart_idx to the mapped Mswi array; volatile preserves the MMIO
        // side effect and each element is a distinct hart register.
        unsafe { self.mswi.0[hart_idx].0.get().write_volatile(1) }
    }

    #[inline]
    pub(crate) fn clear_msip(&self, hart_idx: usize) {
        // SAFETY: caller bounds hart_idx to the mapped Mswi array; volatile preserves the MMIO
        // side effect and each element is a distinct hart register.
        unsafe { self.mswi.0[hart_idx].0.get().write_volatile(0) }
    }
}

impl SifiveClint {
    #[unsafe(naked)]
    // SAFETY: caller supplies the permanent CLINT base and a validated hart index; naked assembly
    // follows the C register ABI and performs one volatile-equivalent MMIO store before returning.
    pub(crate) unsafe extern "C" fn write_mtimecmp_naked(&self, hart_idx: usize, val: u64) {
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
    // SAFETY: caller supplies the permanent CLINT base and a validated hart index; naked assembly
    // follows the C register ABI and clears exactly that hart's Msip register.
    pub(crate) unsafe extern "C" fn clear_msip_naked(&self, hart_idx: usize) {
        naked_asm!(
            "   slli a1, a1, 2
                    add  a0, a0, a1
                    sw   zero, (a0)
                    ret
                ",
        )
    }
}
