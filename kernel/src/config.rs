// 时钟中断的频率
pub(crate) const TICKS_PER_SEC: usize = 100;

/// Boot、secondary 与 task kernel stack 的统一大小。
pub(crate) const KERNEL_STACK_SIZE: usize = 8192 * 16;
