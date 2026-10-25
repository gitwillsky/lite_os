//! fail-stop 路径的 frame-pointer 回溯：不分配、不加锁，且只解引用已验证的 kernel stack 地址。

/// 单次回溯最多记录的栈帧数。
pub(super) const MAX_FRAMES: usize = 32;

/// 从 `frame` 开始沿 frame pointer 链向上回溯。
///
/// 1. frame pointer 必须 16 字节对齐，且 record 所在的 32 字节窗口整个位于某个 kernel stack；
/// 2. 链必须严格向栈顶增长，单步跨度不得超过一个 kernel stack；
/// 3. 任一条件不满足就停止，已采集的帧仍然有效。
///
/// # Parameters
///
/// - `frame`: 起点 frame pointer，通常是 [`crate::arch::backtrace::frame_pointer`]。
/// - `frames`: 接收返回地址的缓冲。
///
/// # Returns
///
/// 写入 `frames` 的返回地址个数。
pub(super) fn capture(frame: usize, frames: &mut [usize; MAX_FRAMES]) -> usize {
    let mut count = 0;
    let mut frame = frame;
    while count < MAX_FRAMES
        && frame >= 16
        && frame.is_multiple_of(16)
        && crate::memory::is_kernel_stack_address(frame - 16)
        && crate::memory::is_kernel_stack_address(frame + 15)
    {
        // SAFETY: 上面已验证 `frame - 16..frame + 16` 位于已映射的 kernel stack。
        let (next, address) = unsafe { crate::arch::backtrace::read_frame(frame) };
        if address == 0 {
            break;
        }
        frames[count] = address;
        count += 1;
        if next <= frame || next - frame > crate::memory::KERNEL_STACK_SIZE {
            break;
        }
        frame = next;
    }
    count
}
