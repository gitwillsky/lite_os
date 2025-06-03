use core::arch::asm;

use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress};
use page_table::{PTEFlags, PageTableEntry};

use crate::board;

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

    // enable mmu
    let stap_val = (8 << 60) | root_ppn.as_usize(); // sv39
    unsafe {
        asm!("csrw satp, {}", in(reg) stap_val);
        asm!("sfence.vma zero, zero")
    }
    println!("Memory module initialized");
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
    let root_frame = frame_allocator::alloc().expect("can not allocate kernel page table memory");
    let root_ppn = root_frame.ppn;
    // ignore drop this frame, because the kernel always need it
    core::mem::forget(root_frame);

    // map kernel text section
    map_page_table_region(
        root_ppn,
        (stext as usize).into(),
        (stext as usize).into(),
        etext as usize - stext as usize,
        PTEFlags::X | PTEFlags::R | PTEFlags::G,
    );

    // map kernel ready only data section
    map_page_table_region(
        root_ppn,
        (srodata as usize).into(),
        (srodata as usize).into(),
        erodata as usize - srodata as usize,
        PTEFlags::G | PTEFlags::R,
    );

    // map kernel data section
    map_page_table_region(
        root_ppn,
        (sdata as usize).into(),
        (edata as usize).into(),
        edata as usize - sdata as usize,
        PTEFlags::G | PTEFlags::R | PTEFlags::W,
    );

    // map kernel bss section
    map_page_table_region(
        root_ppn,
        (sbss as usize).into(),
        (sbss as usize).into(),
        ebss as usize - sbss as usize,
        PTEFlags::G | PTEFlags::R | PTEFlags::W,
    );

    // map kernel stack section
    let guard_size = config::PAGE_SIZE; // 预留一页作为内核栈哨兵页
    map_page_table_region(
        root_ppn,
        (boot_stack_bottom as usize + guard_size).into(),
        (boot_stack_bottom as usize + guard_size).into(),
        boot_stack_top as usize - boot_stack_bottom as usize,
        PTEFlags::G | PTEFlags::R | PTEFlags::W,
    );

    // map other memory region
    let ekernel: usize = ekernel as usize;
    map_page_table_region(
        root_ppn,
        ekernel.into(),
        ekernel.into(),
        memory_end_addr.as_usize() - ekernel,
        PTEFlags::G | PTEFlags::W | PTEFlags::R,
    );

    root_ppn
}
