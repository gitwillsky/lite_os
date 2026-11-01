//! Linux 程序浮点/SIMD 状态的显式保存与恢复。
//!
//! 服务代码按 softfloat ABI 编译、从不触碰向量寄存器，因此在 restricted 退出后 Q0-Q31、
//! FPSR、FPCR 仍是 Linux 程序的值。这里的例程是服务访问它们的唯一入口。

/// 一份完整的 FPSIMD 状态（布局与 `user_fpsimd_state` 相同）。
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FpState {
    pub(crate) vregs: [u128; 32],
    pub(crate) fpsr: u32,
    pub(crate) fpcr: u32,
}

core::arch::global_asm!(
    ".arch armv8-a+simd",
    ".text",
    ".balign 4",
    // x0：fpsr/fpcr 两个 32 位字；x1：16 字节对齐的 32 个向量寄存器。
    ".global linuxd_fp_capture",
    "linuxd_fp_capture:",
    "mrs x9, fpsr",
    "mrs x10, fpcr",
    "str w9, [x0, #0]",
    "str w10, [x0, #4]",
    "stp q0, q1, [x1, #0]",
    "stp q2, q3, [x1, #32]",
    "stp q4, q5, [x1, #64]",
    "stp q6, q7, [x1, #96]",
    "stp q8, q9, [x1, #128]",
    "stp q10, q11, [x1, #160]",
    "stp q12, q13, [x1, #192]",
    "stp q14, q15, [x1, #224]",
    "stp q16, q17, [x1, #256]",
    "stp q18, q19, [x1, #288]",
    "stp q20, q21, [x1, #320]",
    "stp q22, q23, [x1, #352]",
    "stp q24, q25, [x1, #384]",
    "stp q26, q27, [x1, #416]",
    "stp q28, q29, [x1, #448]",
    "stp q30, q31, [x1, #480]",
    "ret",
    ".global linuxd_fp_restore",
    "linuxd_fp_restore:",
    "ldr w9, [x0, #0]",
    "ldr w10, [x0, #4]",
    "msr fpsr, x9",
    "msr fpcr, x10",
    "ldp q0, q1, [x1, #0]",
    "ldp q2, q3, [x1, #32]",
    "ldp q4, q5, [x1, #64]",
    "ldp q6, q7, [x1, #96]",
    "ldp q8, q9, [x1, #128]",
    "ldp q10, q11, [x1, #160]",
    "ldp q12, q13, [x1, #192]",
    "ldp q14, q15, [x1, #224]",
    "ldp q16, q17, [x1, #256]",
    "ldp q18, q19, [x1, #288]",
    "ldp q20, q21, [x1, #320]",
    "ldp q22, q23, [x1, #352]",
    "ldp q24, q25, [x1, #384]",
    "ldp q26, q27, [x1, #416]",
    "ldp q28, q29, [x1, #448]",
    "ldp q30, q31, [x1, #480]",
    "ret",
);

// SAFETY: 本模块汇编定义两个例程，签名对应 control[2] 与 vregs[32]，调用点保证布局与对齐。
unsafe extern "C" {
    fn linuxd_fp_capture(control: *mut u32, vregs: *mut u128);
    fn linuxd_fp_restore(control: *const u32, vregs: *const u128);
}

/// 捕获当前线程的硬件浮点状态。
pub(crate) fn capture() -> FpState {
    let mut state = FpState::default();
    let mut control = [0u32; 2];
    // SAFETY: 两个输出缓冲区由本函数独占、满足对齐；例程只写它们。
    unsafe { linuxd_fp_capture(control.as_mut_ptr(), state.vregs.as_mut_ptr()) };
    state.fpsr = control[0];
    state.fpcr = control[1];
    state
}

/// 把浮点状态装入当前线程的硬件寄存器。
pub(crate) fn restore(state: &FpState) {
    let control = [state.fpsr, state.fpcr];
    // SAFETY: 输入缓冲区有效且对齐；例程只读它们。
    unsafe { linuxd_fp_restore(control.as_ptr(), state.vregs.as_ptr()) };
}

/// 捕获到信号帧的 FPSIMD 记录体（`fpsr`、`fpcr` 后接 8 字节偏移处的向量寄存器）。
///
/// # Safety
///
/// `body` 指向信号帧内 16 字节对齐记录头之后的 528 字节记录体，调用期间独占。
pub(super) unsafe fn capture_signal(body: *mut u8) {
    // SAFETY: 由调用者保证记录体有效；向量区位于记录体 +8，满足 16 字节对齐。
    unsafe { linuxd_fp_capture(body.cast(), body.add(8).cast()) };
}

/// 从信号帧的 FPSIMD 记录体恢复。
///
/// # Safety
///
/// `body` 指向已验证、16 字节对齐记录头之后的 528 字节记录体。
pub(super) unsafe fn restore_signal(body: *const u8) {
    // SAFETY: 由调用者保证记录体有效且对齐。
    unsafe { linuxd_fp_restore(body.cast(), body.add(8).cast()) };
}
