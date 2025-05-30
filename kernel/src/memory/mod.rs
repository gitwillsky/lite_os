use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress};
use page_table::{PTEFlags, PageTableEntry};

use crate::board;

pub mod address;
pub mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod page_table;

unsafe extern "C" {
    static mut ekernel: usize;
}

pub fn init() {
    let kernel_end_addr: PhysicalAddress = unsafe { ekernel.into() };
    let memory_end_addr: PhysicalAddress = board::get_board_info().mem.end.into();
    println!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    println!("memory_end_addr: {:#x}", memory_end_addr.as_usize());
    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);
    println!("Memory module initialized");
}

fn map_page_table_region(
    root_ppn: PhysicalPageNumber,
    va_start: VirtualAddress,
    pa_start: PhysicalAddress,
    size: usize,
    flags: PTEFlags,
) {
    assert!(va_start.is_aligned());
    assert!(pa_start.is_aligned());
    assert_eq!(size % config::PAGE_SIZE, 0);

    let pages = size / config::PAGE_SIZE;

    for i in 0..pages {
        let va: VirtualAddress = (usize::from(va_start) + i * config::PAGE_SIZE).into();
        let pa: PhysicalAddress = (usize::from(pa_start) + i * config::PAGE_SIZE).into();

        let l2_table_entry_addr =
            PhysicalAddress::from(root_ppn).as_usize() + (va.as_usize() >> 30 & 0x1FF) * 8;
        let mut l2_page_entry = unsafe { *(l2_table_entry_addr as *mut PageTableEntry) };

        let l1_table_ppn: PhysicalPageNumber = if l2_page_entry.is_leaf()
            || !l2_page_entry.is_valid()
        {
            // reset
            let new_l1_table_ppn = frame_allocator::alloc().expect("can not allocate l1 table ppn");

            new_l1_table_ppn.ppn
        } else {
            l2_page_entry.ppn()
        };
    }

    // 将 va 转换为页表项
    let root_addr = PhysicalAddress::from(root_ppn);
}

fn init_kernel_page_table() {
    let root_frame = frame_allocator::alloc().expect("can not allocate kernel page table memory");
}
