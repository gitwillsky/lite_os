use core::arch::asm;

use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress};
use page_table::{PTEFlags, PageTableEntry};

use crate::board;

pub mod address;
pub mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod page_table;

// 添加调试宏
#[macro_export]
macro_rules! debug_breakpoint {
    () => {
        unsafe {
            core::arch::asm!("ebreak");
        }
    };
}

// 带消息的调试断点函数
#[inline(never)]
pub fn debug_break_with_msg(msg: &str) {
    println!("DEBUG BREAKPOINT: {}", msg);
    unsafe {
        core::arch::asm!("ebreak");
    }
}

// 条件断点宏
#[macro_export]
macro_rules! debug_break_if {
    ($condition:expr, $msg:expr) => {
        if $condition {
            crate::memory::debug_break_with_msg($msg);
        }
    };
}

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
    debug_breakpoint!();

    let root_ppn = init_kernel_page_table();

    // enable mmu
    // let stap_val = (8 << 60) | root_ppn.as_usize();
    // unsafe {
    //     asm!("csrw satp, {}", in(reg) stap_val);
    //     asm!("sfence.vma zero, zero")
    // }
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
        println!("--------------------");
        let va: VirtualAddress = (usize::from(va_start) + i * config::PAGE_SIZE).into();
        let pa: PhysicalAddress = (usize::from(pa_start) + i * config::PAGE_SIZE).into();

        let l2_table_entry_addr =
            PhysicalAddress::from(root_ppn).as_usize() + (va.as_usize() >> 30 & 0x1FF) * 8;
        println!(
            "{:#x} root_ppn: {} va: {:#x}",
            l2_table_entry_addr,
            root_ppn.as_usize(),
            va.as_usize()
        );
        let l2_page_entry = unsafe { &mut *(l2_table_entry_addr as *mut PageTableEntry) };
        println!("l2_page_entry: {:#?}", l2_page_entry);

        let l1_table_ppn: PhysicalPageNumber = if l2_page_entry.is_leaf()
            || !l2_page_entry.is_valid()
        {
            // reset
            let new_l1_table_ppn = frame_allocator::alloc().expect("can not allocate l1 table ppn");
            let ppn = new_l1_table_ppn.ppn;
            println!("new l1 table ppn: {}", ppn.as_usize());
            *l2_page_entry = PageTableEntry::new(ppn, PTEFlags::V);
            println!("l2_page_entry: {:#?}", &l2_page_entry);

            // 注意：这里需要确保 new_l1_table_ppn 的生命周期管理
            // 当前实现存在内存泄漏风险，因为 FrameTracker 会在函数结束时释放页帧
            // TODO: 需要在页表结构中持有 FrameTracker 的所有权
            core::mem::forget(new_l1_table_ppn); // 临时解决方案：阻止自动释放
            ppn
        } else {
            println!("6666");
            l2_page_entry.ppn()
        };
        println!("1111");

        // find in l1 table
        let l1_table_entry_addr =
            PhysicalAddress::from(l1_table_ppn).as_usize() + (va.as_usize() >> 21 & 0x1FF) * 8;
        let l1_table_entry = unsafe { &mut *(l1_table_entry_addr as *mut PageTableEntry) };
        println!(
            "l1 table addr {:#x} l1table_ppn: {} va: {:#x} l1_table_entry: {:#?}",
            l1_table_entry_addr,
            l1_table_ppn.as_usize(),
            va.as_usize(),
            l1_table_entry,
        );
        let l0_table_ppn: PhysicalPageNumber = if l1_table_entry.is_leaf()
            || !l1_table_entry.is_valid()
        {
            println!("5555");

            let new_l0_table_ppn = frame_allocator::alloc().expect("can not allocate l0 table ppn");
            let ppn = new_l0_table_ppn.ppn;
            *l1_table_entry = PageTableEntry::new(ppn, PTEFlags::V);
            core::mem::forget(new_l0_table_ppn);
            println!("3333");
            ppn
        } else {
            println!("444");
            l1_table_entry.ppn()
        };
        println!("2");

        let l0_table_entry_addr =
            PhysicalAddress::from(l0_table_ppn).as_usize() + (va.as_usize() >> 12 & 0x1FF) * 8;
        let l0_table_entry = unsafe { &mut *(l0_table_entry_addr as *mut PageTableEntry) };
        *l0_table_entry = PageTableEntry::new(pa.page_number(), PTEFlags::V | flags);
        println!("3");
    }
}

fn init_kernel_page_table() -> PhysicalPageNumber {
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
        (ebss as usize).into(),
        ebss as usize - sbss as usize,
        PTEFlags::G | PTEFlags::R | PTEFlags::W,
    );

    // map kernel stack section
    map_page_table_region(
        root_ppn,
        (ebss as usize).into(),
        (ebss as usize).into(),
        boot_stack_top as usize - ebss as usize,
        PTEFlags::G | PTEFlags::R | PTEFlags::W,
    );

    root_ppn
}
