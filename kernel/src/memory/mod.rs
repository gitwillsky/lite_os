use core::arch::asm;

use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress};
use page_table::{PTEFlags, PageTableEntry};

use crate::{board, memory::mm::MemorySet};

pub mod address;
mod config;
pub mod frame_allocator;
pub mod heap_allocator;
mod mm;
mod page_table;

unsafe extern "C" {
    fn skernel();

    fn stext();
    fn etext();

    fn srodata();
    fn erodata();

    fn sdata();
    fn edata();

    fn sbss();
    fn ebss();

    fn boot_stack_bottom();
    fn boot_stack_top();

    fn ekernel();
}

pub fn init() {
    let kernel_end_addr: PhysicalAddress = (ekernel as usize).into();
    let memory_end_addr: PhysicalAddress = board::get_board_info().mem.end.into();
    println!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    println!("memory_end_addr: {:#x}", memory_end_addr.as_usize());
    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);

    let root_ppn = init_kernel_page_table(memory_end_addr);
}

fn map_page_table_region(
    root_ppn: PhysicalPageNumber,
    va_start: VirtualAddress,
    pa_start: PhysicalAddress,
    size: usize,
    flags: PTEFlags,
) {
}

fn init_kernel_page_table(memory_end_addr: PhysicalAddress) -> PhysicalPageNumber {
    let mut memory_set = MemorySet::new();
}
