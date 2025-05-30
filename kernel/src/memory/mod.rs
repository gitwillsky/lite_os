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
    static skernel: usize;

    static stext: usize;
    static etext: usize;

    static srodata: usize;
    static erodata: usize;

    static sdata: usize;
    static edata: usize;

    static sbss: usize;
    static ebss: usize;

    static boot_stack_bottom: usize;
    static boot_stack_top: usize;

    fn ekernel();
}

pub fn init() {
    println!("{:#x}", unsafe { ekernel as usize });
    let kernel_end_addr: PhysicalAddress = unsafe { ekernel as usize }.into();
    let memory_end_addr: PhysicalAddress = board::get_board_info().mem.end.into();
    println!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    println!("memory_end_addr: {:#x}", memory_end_addr.as_usize());
    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);

    let root_ppn = init_kernel_page_table();

    // enable mmu
    let stap_val = (8 << 60) | root_ppn.as_usize();
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

            // 注意：这里需要确保 new_l1_table_ppn 的生命周期管理
            // 当前实现存在内存泄漏风险，因为 FrameTracker 会在函数结束时释放页帧
            // TODO: 需要在页表结构中持有 FrameTracker 的所有权
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
        *l0_table_entry = PageTableEntry::new(pa.page_number(), PTEFlags::V | flags)
    }
}

fn init_kernel_page_table() -> PhysicalPageNumber {
    let root_frame = frame_allocator::alloc().expect("can not allocate kernel page table memory");
    let root_ppn = root_frame.ppn;
    // ignore drop this frame, because the kernel always need it
    core::mem::forget(root_frame);

    // map kernel text section
    unsafe {
        map_page_table_region(
            root_ppn,
            stext.into(),
            stext.into(),
            etext - stext,
            PTEFlags::X | PTEFlags::R | PTEFlags::G,
        );
    }

    // map kernel ready only data section
    unsafe {
        map_page_table_region(
            root_ppn,
            srodata.into(),
            srodata.into(),
            erodata - srodata,
            PTEFlags::G | PTEFlags::R,
        );
    }

    // map kernel data section
    unsafe {
        map_page_table_region(
            root_ppn,
            sdata.into(),
            edata.into(),
            sdata - edata,
            PTEFlags::G | PTEFlags::R | PTEFlags::W,
        );
    }

    // map kernel bss section
    unsafe {
        map_page_table_region(
            root_ppn,
            sbss.into(),
            ebss.into(),
            ebss- sbss,
            PTEFlags::G | PTEFlags::R | PTEFlags::W,
        );
    }

    // map kernel stack section
    unsafe {
        map_page_table_region(
            root_ppn,
            ebss.into(),
            ebss.into(),
            boot_stack_top - ebss,
            PTEFlags::G | PTEFlags::R | PTEFlags::W,
        );
    }

    root_ppn
}
