/// 特权软件入口。
pub(crate) const KERNEL_ENTRY: usize = 0x80200000;
/// 每个硬件线程设置 16 KiB 栈空间。
pub(crate) const STACK_SIZE_PER_HART: usize = 16 * 1024;
/// firmware 可索引的最大 hart ID 容量，不表示 DTB 实际启用核数。
pub(crate) const MAX_SUPPORTED_HARTS: usize = 8;
