use core::arch::asm;

use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress};
use page_table::{PTEFlags, PageTableEntry};

use crate::board;

pub mod address;
pub mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod page_table;

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
    assert!(va_start.is_aligned());
    assert!(pa_start.is_aligned());
    assert_eq!(size % config::PAGE_SIZE, 0);

    let pages = size / config::PAGE_SIZE;

    for i in 0..pages {
        let va: VirtualAddress = (usize::from(va_start) + i * config::PAGE_SIZE).into();
        let pa: PhysicalAddress = (usize::from(pa_start) + i * config::PAGE_SIZE).into();

        let l2_table_entry_addr =
            PhysicalAddress::from(root_ppn).as_usize() + (va.as_usize() >> 30 & 0x1FF) * 8;

        let l2_page_entry = unsafe { &mut *(l2_table_entry_addr as *mut PageTableEntry) };

        let l1_table_ppn: PhysicalPageNumber = if l2_page_entry.is_leaf()
            || !l2_page_entry.is_valid()
        {
            // reset
            let new_l1_table_ppn = frame_allocator::alloc().expect("can not allocate l1 table ppn");
            let ppn = new_l1_table_ppn.ppn;
            *l2_page_entry = PageTableEntry::new(ppn, PTEFlags::V);

            core::mem::forget(new_l1_table_ppn); // 临时解决方案：阻止自动释放
            ppn
        } else {
            l2_page_entry.ppn()
        };

        // find in l1 table
        let l1_table_entry_addr =
            PhysicalAddress::from(l1_table_ppn).as_usize() + (va.as_usize() >> 21 & 0x1FF) * 8;
        let l1_table_entry = unsafe { &mut *(l1_table_entry_addr as *mut PageTableEntry) };
        let l0_table_ppn: PhysicalPageNumber = if l1_table_entry.is_leaf()
            || !l1_table_entry.is_valid()
        {
            let new_l0_table_ppn = frame_allocator::alloc().expect("can not allocate l0 table ppn");
            let ppn = new_l0_table_ppn.ppn;
            *l1_table_entry = PageTableEntry::new(ppn, PTEFlags::V);
            core::mem::forget(new_l0_table_ppn);
            ppn
        } else {
            l1_table_entry.ppn()
        };

        let l0_table_entry_addr =
            PhysicalAddress::from(l0_table_ppn).as_usize() + (va.as_usize() >> 12 & 0x1FF) * 8;
        let l0_table_entry = unsafe { &mut *(l0_table_entry_addr as *mut PageTableEntry) };
        *l0_table_entry = PageTableEntry::new(pa.page_number(), PTEFlags::V | flags);
    }
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
