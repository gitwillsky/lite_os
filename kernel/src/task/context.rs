/// @description 可被调度器切换的 kernel psABI callee-saved context。
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskContext {
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

const _: () = {
    use core::mem::{offset_of, size_of};
    const WORD: usize = size_of::<usize>();
    assert!(offset_of!(TaskContext, fs) == 14 * WORD);
    assert!(offset_of!(TaskContext, fcsr) == 26 * WORD);
    assert!(size_of::<TaskContext>() == 27 * WORD);
};

impl TaskContext {
    pub(crate) fn zero_init() -> Self {
        Self {
            ra: 0,
            kernel_sp: 0,
            s: [0; 12],
            fs: [0; 12],
            fcsr: 0,
        }
    }

    pub(crate) fn goto_trap_return(kernel_sp: usize, trap_return: usize) -> Self {
        Self {
            ra: trap_return,
            kernel_sp,
            s: [0; 12],
            fs: [0; 12],
            fcsr: 0,
        }
    }

    /// 设置返回地址
    pub(crate) fn set_ra(&mut self, ra: usize) {
        self.ra = ra;
    }
}
