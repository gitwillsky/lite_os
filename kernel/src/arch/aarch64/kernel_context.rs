/// Kernel AAPCS64 callee-saved state plus the eager FP/ASIMD task image.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct KernelContext {
    x19_x30: [usize; 12],
    kernel_sp: usize,
    _padding: usize,
    q: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

/// Typed continuation entered after the first architecture context restore.
pub(crate) type KernelResume = fn() -> !;

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(KernelContext, kernel_sp) == 96);
    assert!(offset_of!(KernelContext, q) == 112);
    assert!(offset_of!(KernelContext, fpcr) == 624);
    assert!(size_of::<KernelContext>() == 640);
};

impl KernelContext {
    /// Construct a context without a continuation.
    pub(crate) fn zero_init() -> Self {
        Self {
            x19_x30: [0; 12],
            kernel_sp: 0,
            _padding: 0,
            q: [0; 32],
            fpcr: 0,
            fpsr: 0,
        }
    }

    /// Construct a context whose first restore returns to `trap_return` through x30.
    pub(crate) fn goto_trap_return(kernel_sp: usize, trap_return: KernelResume) -> Self {
        let mut context = Self::zero_init();
        context.x19_x30[11] = trap_return as usize;
        context.kernel_sp = kernel_sp;
        context
    }

    /// @description 构造 clone/fork/vfork child，并继承 calling task 的 live FP/NEON image。
    /// @param kernel_sp child 独占 kernel stack top。
    /// @param trap_return child 首次 restore 后进入的 continuation。
    /// @return integer continuation 已初始化、vector state 来自当前 CPU live task 的 context。
    pub(crate) fn clone_for_trap_return(kernel_sp: usize, trap_return: KernelResume) -> Self {
        let mut context = Self::goto_trap_return(kernel_sp, trap_return);
        // SAFETY: context is aligned, uniquely owned and unpublished; the current task exclusively
        // owns the calling CPU's live vector file throughout clone preparation.
        unsafe { super::fp_state::capture_clone(&mut context) };
        context
    }

    /// Replace the first-restore continuation carried in x30.
    pub(crate) fn set_resume_target(&mut self, target: KernelResume) {
        self.x19_x30[11] = target as usize;
    }
}

// SAFETY: switch.S exports this symbol with the KernelContext layout proven by the const offsets.
unsafe extern "C" {
    fn __switch(current: *mut KernelContext, next: *const KernelContext);
}

/// Save the calling kernel continuation and restore another task context.
// SAFETY: caller owns an aligned live save target and keeps the restore source alive until the
// transfer has completed; the pointers must not overlap mutably.
pub(crate) unsafe fn switch_kernel_context(
    current: *mut KernelContext,
    next: *const KernelContext,
) {
    // SAFETY: caller upholds the lifetime, alignment and unique-save contract.
    unsafe { __switch(current, next) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resume() -> ! {
        panic!("test continuation must not run")
    }

    #[test]
    fn initial_context_owns_a_zero_fp_neon_image() {
        let context = KernelContext::zero_init();
        assert_eq!(context.x19_x30, [0; 12]);
        assert_eq!(context.kernel_sp, 0);
        assert_eq!(context.q, [0; 32]);
        assert_eq!(context.fpcr, 0);
        assert_eq!(context.fpsr, 0);
    }

    #[test]
    fn first_restore_initializes_only_the_integer_continuation() {
        let context = KernelContext::goto_trap_return(0x1234_0000, resume);
        assert_eq!(context.x19_x30[..11], [0; 11]);
        assert_eq!(context.x19_x30[11], resume as *const () as usize);
        assert_eq!(context.kernel_sp, 0x1234_0000);
        assert_eq!(context.q, [0; 32]);
        assert_eq!(context.fpcr, 0);
        assert_eq!(context.fpsr, 0);
    }

    #[test]
    fn clone_constructor_uses_the_architecture_capture_boundary() {
        let mut context = KernelContext::clone_for_trap_return(0x2000, resume);
        assert_eq!(context.kernel_sp, 0x2000);
        assert_eq!(context.q, [0; 32]);
        context.set_resume_target(resume);
        assert_eq!(context.x19_x30[11], resume as *const () as usize);
    }
}
