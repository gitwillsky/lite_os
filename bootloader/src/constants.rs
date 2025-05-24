/// 特权软件入口
pub(crate) const KERNEL_ENTRY: usize = 0x80200000;
/// 每个硬件线程设置 16 KiB 栈空间
pub(crate) const STACK_SIZE_PER_HART: usize = 16 * 1024;
/// qemu-virt 最多 8 个硬件线程
pub(crate) const MAX_HART_NUM: usize = 8;
