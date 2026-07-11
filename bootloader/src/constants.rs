/// 特权软件入口。
pub(crate) const KERNEL_ENTRY: usize = 0x80200000;
/// 每个硬件线程设置 16 KiB 栈空间。
pub(crate) const STACK_SIZE_PER_HART: usize = 16 * 1024;
/// SBI 单字 hart mask 能表达的 hart ID 数量。
pub(crate) const HART_MASK_BITS: usize = usize::BITS as usize;
