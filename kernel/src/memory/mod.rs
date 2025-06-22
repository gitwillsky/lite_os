use address::PhysicalAddress;
use spin::{Mutex, Once};

use crate::{
    board,
    memory::mm::{MapArea, MapPermission, MemorySet},
};

pub mod address;
mod config;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod mm;
pub mod page_table;

pub use config::*;
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
    pub fn strampoline();
}

pub static KERNEL_SPACE: Once<Mutex<MemorySet>> = Once::new();

pub fn init() {
    let kernel_end_addr: PhysicalAddress = (ekernel as usize).into();
    let memory_end_addr: PhysicalAddress = board::get_board_info().mem.end.into();
    println!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    println!("memory_end_addr: {:#x}", memory_end_addr.as_usize());
    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);

    KERNEL_SPACE.call_once(|| Mutex::new(init_kernel_space(memory_end_addr)));
    KERNEL_SPACE.wait().lock().active();
}

fn init_kernel_space(memory_end_addr: PhysicalAddress) -> MemorySet {
    let mut memory_set = MemorySet::new();

    memory_set.map_trampoline();

    // kernel text section
    memory_set.push(
        MapArea::new(
            (stext as usize).into(),
            (etext as usize).into(),
            mm::MapType::Identical,
            MapPermission::R | MapPermission::X,
        ),
        None,
    );

    // kernel read only data
    memory_set.push(
        MapArea::new(
            (srodata as usize).into(),
            (erodata as usize).into(),
            mm::MapType::Identical,
            MapPermission::R,
        ),
        None,
    );

    // kernel data
    memory_set.push(
        MapArea::new(
            (sdata as usize).into(),
            (edata as usize).into(),
            mm::MapType::Identical,
            MapPermission::R | MapPermission::W,
        ),
        None,
    );

    // kernel bss section
    memory_set.push(
        MapArea::new(
            (sbss as usize).into(),
            (ebss as usize).into(),
            mm::MapType::Identical,
            MapPermission::R | MapPermission::W,
        ),
        None,
    );

    // other memory
    memory_set.push(
        MapArea::new(
            (ekernel as usize).into(),
            memory_end_addr.as_usize().into(),
            mm::MapType::Identical,
            MapPermission::R | MapPermission::W,
        ),
        None,
    );

    memory_set
}

pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);

    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}
