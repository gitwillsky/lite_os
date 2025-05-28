use crate::board;

pub mod address;
pub mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod page_table;

unsafe extern "C" {
    fn ekernel();
}

pub fn init() {
    let kernel_end_addr = ekernel as usize;
    let memory_end_addr = board::get_board_info().mem.end;
    println!("kernel_end_addr: {:#x}", kernel_end_addr);
    println!("memory_end_addr: {:#x}", memory_end_addr);
    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);
    println!("Memory module initialized");
}
