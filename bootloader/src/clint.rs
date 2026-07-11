use crate::{aclint, constants, hart::hart_id, trap_stack::remote_hsm};
use core::{
    ptr::null_mut,
    sync::atomic::{AtomicPtr, Ordering},
};
use rustsbi::{HartMask, Ipi, SbiRet, Timer};

pub(crate) struct Clint;

// OWNER: CLINT module owns the DTB-selected controller pointer after global initialization.
pub(crate) static CLINT: AtomicPtr<aclint::SifiveClint> = AtomicPtr::new(null_mut());

pub(crate) fn init(base: usize) {
    CLINT.store(base as _, Ordering::Release);
}

fn dtb_hart_mask() -> usize {
    crate::BOARD_INFO.wait().hart_mask
}

impl Ipi for Clint {
    #[inline]
    fn send_ipi(&self, hart_mask: HartMask) -> SbiRet {
        let (mask, base) = hart_mask.into_inner();
        let possible = dtb_hart_mask();
        let selected = if base == usize::MAX {
            possible
        } else if mask == 0 {
            0
        } else if base >= constants::HART_MASK_BITS {
            return SbiRet::invalid_param();
        } else {
            let valid_bits = constants::HART_MASK_BITS - base;
            if valid_bits < usize::BITS as usize && (mask >> valid_bits) != 0 {
                return SbiRet::invalid_param();
            }
            let selected = mask << base;
            if selected & !possible != 0 {
                return SbiRet::invalid_param();
            }
            selected
        };

        // 先验证完整集合，避免对前半目标发送后才发现后半参数非法的部分执行。
        let mut targets = selected;
        while targets != 0 {
            let i = targets.trailing_zeros() as usize;
            targets &= targets - 1;
            if !remote_hsm(i).is_some_and(|hsm| hsm.allow_ipi()) {
                return SbiRet::invalid_param();
            }
        }
        let mut targets = selected;
        while targets != 0 {
            let i = targets.trailing_zeros() as usize;
            targets &= targets - 1;
            set_msip(i);
        }
        SbiRet::success(0)
    }
}

impl Timer for Clint {
    #[inline]
    fn set_timer(&self, time_value: u64) {
        // SAFETY: firmware runs in M-mode; CLINT is published before SBI service exposure and
        // current hart_id indexes the DTB-validated controller mapping.
        unsafe {
            riscv::register::mip::clear_stimer();
            (*CLINT.load(Ordering::Acquire)).write_mtimecmp(hart_id(), time_value)
        }
    }
}

#[inline]
pub(crate) fn set_msip(hart_idx: usize) {
    assert!(
        hart_idx < constants::HART_MASK_BITS,
        "CLINT hart index out of range"
    );
    // SAFETY: initialization publishes a non-null DTB-validated CLINT pointer before any IPI;
    // explicit assertion bounds the destination register index.
    unsafe { &*CLINT.load(Ordering::Acquire) }.set_msip(hart_idx);
}

#[inline]
pub(crate) fn clear_msip() {
    // SAFETY: initialization publishes a non-null CLINT pointer before hart execution reaches
    // this path, and current hart_id belongs to the validated firmware hart set.
    unsafe { &*CLINT.load(Ordering::Acquire) }.clear_msip(hart_id());
}

#[inline]
pub(crate) fn clear() {
    loop {
        // SAFETY: `as_ref` is used only to poll initialization; a non-null value is a permanent
        // DTB-validated MMIO mapping stored with Release ordering.
        if let Some(clint) = unsafe { CLINT.load(Ordering::Acquire).as_ref() } {
            clint.clear_msip(hart_id());
            clint.write_mtimecmp(hart_id(), u64::MAX);
            break;
        } else {
            continue;
        }
    }
}
