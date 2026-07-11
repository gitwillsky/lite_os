pub(crate) const MAX_HEAP_SIZE: usize = 4 * 1024 * 1024; // 4MB

pub(crate) const PAGE_SIZE: usize = 4096; // 4KB
pub(crate) const PHYSICAL_ADDRESS_WIDTH: usize = 56; // sv39
pub(crate) const VIRTUAL_ADDRESS_WIDTH: usize = 39; // sv39
pub(crate) const PAGE_OFFSET_WIDTH: usize = 12; // 页内偏移, 一页 4kb 2^12
pub(crate) const PTE_FLAGS_WIDTH: usize = 10; // Page Table Entry Flags width
pub(crate) const PPN_WIDTH: usize = PHYSICAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;
pub(crate) const VPN_WIDTH: usize = VIRTUAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;

// 16kb user stack
pub(crate) const USER_STACK_SIZE: usize = 8192 * 32; // increase to 256KB for deep recursion
pub(crate) const KERNEL_STACK_SIZE: usize = 8192 * 16; // boot/task/dynamic hart stack 的统一大小

pub(crate) const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
pub(crate) const TRAP_CONTEXT: usize = TRAMPOLINE - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE: usize = TRAP_CONTEXT - PAGE_SIZE;
