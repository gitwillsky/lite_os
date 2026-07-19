//! @description AArch64 signal/clone/exec 边界对 live FP/NEON register file 的唯一访问 seam。

use super::kernel_context::KernelContext;

#[repr(C, align(16))]
struct ZeroFpState([u8; 536]);

// OWNER: this immutable image is the sole exec-time FP/NEON reset source; assembly only reads it
// at the explicit exec commit boundary, so no replicated live vector state exists in Rust.
static ZERO_FP_STATE: ZeroFpState = ZeroFpState([0; 536]);

// SAFETY: fp_boundary.S owns these symbols and each routine closes CPACR_EL1.FPEN before return.
unsafe extern "C" {
    fn __aarch64_signal_fp_capture(state: *mut u8);
    fn __aarch64_signal_fp_restore(state: *const u8);
    fn __aarch64_clone_fp_capture(context: *mut KernelContext);
    fn __aarch64_exec_fp_reset(state: *const u8);
}

/// @description 在 signal delivery boundary 捕获 live fpsr/fpcr/Q0-Q31。
/// @param state 16-byte aligned 528-byte FPSIMD record body。
/// @return 无返回值；helper 返回前重新关闭 FPEN。
/// @errors pointer 不满足 alignment/size/unique-write 会破坏 kernel memory。
// SAFETY: caller provides the unique aligned FPSIMD payload inside an owned SignalFrame.
pub(super) unsafe fn capture_signal(state: *mut u8) {
    // SAFETY: caller owns the complete aligned output payload for the duration of this call.
    unsafe { __aarch64_signal_fp_capture(state) };
}

/// @description 在 sigreturn boundary 恢复 live fpsr/fpcr/Q0-Q31。
/// @param state 已完整验证、16-byte aligned 的 528-byte FPSIMD record body。
/// @return 无返回值；helper 返回前重新关闭 FPEN。
/// @errors pointer 不满足 alignment/size/readability 会读取非法 kernel memory。
// SAFETY: caller retains a validated aligned SignalFrame for the duration of this call.
pub(super) unsafe fn restore_signal(state: *const u8) {
    // SAFETY: caller guarantees the immutable payload remains live and aligned.
    unsafe { __aarch64_signal_fp_restore(state) };
}

/// @description 为 clone/fork/vfork child 捕获当前 live FP/NEON task image。
/// @param context 尚未发布且由 caller 独占的 child KernelContext。
/// @return 无返回值；integer continuation 字段保持不变，helper 返回前关闭 FPEN。
/// @errors context 不满足 alignment/unique-write 会破坏 child state。
// SAFETY: caller owns an aligned unpublished KernelContext.
pub(super) unsafe fn capture_clone(context: *mut KernelContext) {
    // SAFETY: caller provides the unique child context and assembly uses proven fixed offsets.
    unsafe { __aarch64_clone_fp_capture(context) };
}

/// @description exec commit 后把当前 CPU 的 live FP/NEON task image 清零。
/// @return 无返回值；Q0-Q31、FPSR、FPCR 均为零，返回 Rust 前 FPEN 关闭。
pub(crate) fn reset_live() {
    // SAFETY: immutable aligned zero state is process-lifetime static; helper only reads it and
    // mutates the calling CPU's live vector file at the explicit exec commit boundary.
    // The Linux record places vregs eight bytes after fpsr; shifting the static pointer by eight
    // keeps every Q pair 16-byte aligned exactly like a real FPSIMD record.
    unsafe { __aarch64_exec_fp_reset(ZERO_FP_STATE.0.as_ptr().add(8)) };
}
