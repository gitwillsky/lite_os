/// @description 可被调度器切换的 kernel psABI callee-saved context。
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct KernelContext {
    /// return address
    ra: usize,
    /// kernel stack pointer of app
    kernel_sp: usize,
    /// callee saved registers: s 0..11
    s: [usize; 12],
    /// LP64D callee-saved floating-point registers fs0..fs11。
    fs: [u64; 12],
    /// floating-point control/status register，具有线程存储期。
    fcsr: usize,
}

/// @description Architecture context restore 后进入的 typed Rust continuation。
pub(crate) type KernelResume = fn() -> !;

const _: () = {
    use core::mem::{offset_of, size_of};
    const WORD: usize = size_of::<usize>();
    assert!(offset_of!(KernelContext, fs) == 14 * WORD);
    assert!(offset_of!(KernelContext, fcsr) == 26 * WORD);
    assert!(size_of::<KernelContext>() == 27 * WORD);
};

impl KernelContext {
    /// @description 构造尚无 continuation 的零初始化 context。
    /// @return 全部 integer/floating state 为零的 context。
    pub(crate) fn zero_init() -> Self {
        Self {
            ra: 0,
            kernel_sp: 0,
            s: [0; 12],
            fs: [0; 12],
            fcsr: 0,
        }
    }

    /// @description 构造首次 restore 后进入 trap-return continuation 的 context。
    /// @param kernel_sp 当前 task 独占 kernel stack top。
    /// @param trap_return 不返回的 typed Rust continuation。
    /// @return 满足 switch.S layout 的 context。
    pub(crate) fn goto_trap_return(kernel_sp: usize, trap_return: KernelResume) -> Self {
        Self {
            ra: trap_return as usize,
            kernel_sp,
            s: [0; 12],
            fs: [0; 12],
            fcsr: 0,
        }
    }

    /// @description 设置 context 首次 restore 后进入的 typed continuation。
    /// @param target 不返回且满足 kernel context ABI 的 Rust function。
    /// @return 无返回值。
    pub(crate) fn set_resume_target(&mut self, target: KernelResume) {
        self.ra = target as usize;
    }
}

// SAFETY: the linked assembly routine obeys the RISC-V LP64D kernel context layout proven above.
unsafe extern "C" {
    fn __switch(current: *mut KernelContext, next: *const KernelContext);
}

/// @description 保存 calling kernel continuation 并恢复另一个 kernel context。
///
/// @param current 保活且独占的 save target。
/// @param next 在切换完成前保持有效的 restore source。
/// @return 恢复 `current` 时返回。
/// @errors 两个指针的 lifetime、alignment 或独占性不成立会破坏内核状态。
// SAFETY: caller must uphold the pointer lifetime, alignment and exclusive-save contract.
pub(crate) unsafe fn switch_kernel_context(
    current: *mut KernelContext,
    next: *const KernelContext,
) {
    // SAFETY: caller upholds the documented lifetime and exclusive-save-target contract.
    unsafe { __switch(current, next) };
}
