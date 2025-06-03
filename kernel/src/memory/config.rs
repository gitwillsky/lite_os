pub const MAX_HEAP_SIZE: usize = 4 * 1024 * 1024; // 4MB

pub const PAGE_SIZE: usize = 4096; // 4KB
pub const PHYSICAL_ADDRESS_WIDTH: usize = 56; // sv39
pub const VIRTUAL_ADDRESS_WIDTH: usize = 39; // sv39
pub const PAGE_OFFSET_WIDTH: usize = 12; // 页内偏移
pub const PTE_FLAGS_WIDTH: usize = 10; // Page Table Entry Flags width
pub const PPN_WIDTH: usize = PHYSICAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;
pub const VPN_WIDTH: usize = VIRTUAL_ADDRESS_WIDTH - PAGE_OFFSET_WIDTH;
