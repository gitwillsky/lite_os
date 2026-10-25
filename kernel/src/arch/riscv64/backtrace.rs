//! RISC-V frame-pointer 回溯机制：内核以 `-C force-frame-pointers=yes` 构建，s0 指向栈帧顶。

use core::arch::asm;

/// 读取当前函数的 frame pointer。
#[inline(always)]
pub(crate) fn frame_pointer() -> usize {
    let frame: usize;
    // SAFETY: 只读取通用寄存器 s0，不访问内存。
    unsafe { asm!("mv {}, s0", out(reg) frame, options(nomem, nostack, preserves_flags)) };
    frame
}

/// 读取 `frame` 处的 frame record。
///
/// # Parameters
///
/// - `frame`: 16 字节对齐、不小于 16、已由调用者确认位于已映射 kernel stack 内的 frame pointer。
///
/// # Returns
///
/// `(上一帧的 frame pointer, 本帧的返回地址)`。
///
/// # Safety
///
/// `frame - 16..frame` 必须可读；RISC-V 把 `{prev s0, ra}` 存放在 frame pointer 之下。
pub(crate) unsafe fn read_frame(frame: usize) -> (usize, usize) {
    let record = (frame - 16) as *const usize;
    // SAFETY: 调用者保证 16 字节可读且对齐。
    unsafe { (record.read(), record.add(1).read()) }
}
