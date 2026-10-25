//! AArch64 frame-record 回溯机制：内核以 `-C force-frame-pointers=yes` 构建，x29 链接每个栈帧。

use core::arch::asm;

/// 读取当前函数的 frame pointer。
#[inline(always)]
pub(crate) fn frame_pointer() -> usize {
    let frame: usize;
    // SAFETY: 只读取通用寄存器 x29，不访问内存。
    unsafe { asm!("mov {}, x29", out(reg) frame, options(nomem, nostack, preserves_flags)) };
    frame
}

/// 读取 `frame` 处的 frame record。
///
/// # Parameters
///
/// - `frame`: 16 字节对齐、已由调用者确认位于已映射 kernel stack 内的 frame pointer。
///
/// # Returns
///
/// `(上一帧的 frame pointer, 本帧的返回地址)`。
///
/// # Safety
///
/// `frame..frame + 16` 必须可读；AArch64 frame record 就是该处的 `{x29, x30}` 对。
// SAFETY: 此接口只由 crash 回溯调用，调用者必须先验证上述完整栈窗口。
pub(crate) unsafe fn read_frame(frame: usize) -> (usize, usize) {
    let record = frame as *const usize;
    // SAFETY: 调用者保证 16 字节可读且对齐。
    unsafe { (record.read(), record.add(1).read()) }
}

/// Read the current kernel stack pointer without dereferencing memory.
#[inline(always)]
pub(crate) fn stack_pointer() -> usize {
    let pointer: usize;
    // SAFETY: reads only the stack-pointer register, without changing SP or accessing memory.
    unsafe { asm!("mov {}, sp", out(reg) pointer, options(nomem, nostack)) };
    pointer
}
