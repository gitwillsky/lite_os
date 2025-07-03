pub const MAX_HEAP_SIZE: usize = 4 * 1024 * 1024; // 4MB

pub const PAGE_SIZE: usize = 4096; // 4KB
pub const PHYSICAL_ADDRESS_WIDTH: usize = 56; // sv39
pub const VIRTUAL_ADDRESS_WIDTH: usize = 39; // sv39
pub const PAGE_OFFSET_WIDTH: usize = 12; // 页内偏移, 一页 4kb 2^12
pub const PTE_FLAGS_WIDTH: usize = 10; // Page Table Entry Flags width
pub const PPN_WIDTH: usize = PHYSICAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;
pub const VPN_WIDTH: usize = VIRTUAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;

// 16kb user stack
pub const USER_STACK_SIZE: usize = 8192 * 2;
pub const KERNEL_STACK_SIZE: usize = 8192 * 2;

pub const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
pub const TRAP_CONTEXT: usize = TRAMPOLINE - PAGE_SIZE;
