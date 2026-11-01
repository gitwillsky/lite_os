// frame allocator 发布前，动态 heap 尚无物理页来源；该 arena 只承载 boot CPU 构造
// CpuTopology 的一次性早期分配。缺失它会让 global allocator 与 frame allocator 循环依赖。
pub(crate) const BOOTSTRAP_HEAP_SIZE: usize = 2 * 1024 * 1024;

pub(crate) use crate::arch::mmu::PAGE_SIZE;

