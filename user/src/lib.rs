#![no_std]
#![feature(linkage)]
#![feature(alloc_error_handler)]

pub mod syscall;
#[macro_use]
pub mod console;

#[macro_use]
extern crate alloc;

mod lang_item;

use core::ptr::addr_of_mut;

use buddy_system_allocator::LockedHeap;
pub use syscall::*;

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

const USER_HEAP_SIZE: usize = 1 * 1024 * 1024; // 1MB

static mut USER_HEAP_MEMORY: [u8; USER_HEAP_SIZE] = [0; USER_HEAP_SIZE];

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_heap_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("allocate heap memory error, layout = {:?}", layout);
}

pub fn init_heap() {
    unsafe {
        // println!
        //     "[heap_allocator::init] heap vaddr={:#x}, size={:#x}",
        //     addr_of_mut!(USER_HEAP_MEMORY) as usize,
        //     USER_HEAP_SIZE
        // );
        HEAP_ALLOCATOR
            .lock()
            .init(addr_of_mut!(USER_HEAP_MEMORY) as usize, USER_HEAP_SIZE);
    }
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
extern "C" fn _start() -> ! {
    init_heap();
    exit(main());
    unreachable!()
}

#[linkage = "weak"] // 弱符号，如果用户没有提供 main 函数，则使用这个默认的
#[unsafe(no_mangle)]
fn main() -> i32 {
    panic!("Can not find app main function")
}
