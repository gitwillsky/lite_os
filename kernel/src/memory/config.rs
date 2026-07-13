// frame allocator 发布前，动态 heap 尚无物理页来源；该 arena 只承载 boot hart 构造
// HartTopology 的一次性早期分配。缺失它会让 global allocator 与 frame allocator 循环依赖。
pub(crate) const BOOTSTRAP_HEAP_SIZE: usize = 2 * 1024 * 1024;

pub(crate) const PAGE_SIZE: usize = 4096; // 4KB
pub(crate) const PHYSICAL_ADDRESS_WIDTH: usize = 56; // sv39
pub(crate) const VIRTUAL_ADDRESS_WIDTH: usize = 39; // sv39
pub(crate) const PAGE_OFFSET_WIDTH: usize = 12; // 页内偏移, 一页 4kb 2^12
pub(crate) const PTE_FLAGS_WIDTH: usize = 10; // Page Table Entry Flags width
pub(crate) const PPN_WIDTH: usize = PHYSICAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;
pub(crate) const VPN_WIDTH: usize = VIRTUAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;

pub(crate) const USER_STACK_SIZE: usize = 8 * 1024 * 1024;
pub(crate) const KERNEL_STACK_SIZE: usize = 8192 * 16; // boot/task/dynamic hart stack 的统一大小

pub(crate) const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
pub(crate) const TRAP_CONTEXT: usize = TRAMPOLINE - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE: usize = TRAP_CONTEXT - PAGE_SIZE;
