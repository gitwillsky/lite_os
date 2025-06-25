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

// 在SV39地址空间中，有效的虚拟地址范围是39位
// 最高有效虚拟地址是 (1 << 39) - 1 = 0x7fffffffff
// 但是为了简化，我们使用接近最高地址的位置
pub const TRAMPOLINE: usize = (1 << (VIRTUAL_ADDRESS_WIDTH - 1)) - PAGE_SIZE;
pub const TRAP_CONTEXT: usize = TRAMPOLINE - PAGE_SIZE;
