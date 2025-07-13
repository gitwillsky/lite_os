use core::{alloc, ptr::addr_of_mut};

use buddy_system_allocator::LockedHeap;

use super::config;

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

static mut KERNEL_HEAP_MEMORY: [u8; config::MAX_HEAP_SIZE] = [0; config::MAX_HEAP_SIZE];

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_heap_alloc_error(layout: alloc::Layout) -> ! {
    panic!("allocate heap memory error, layout = {:?}", layout);
}

pub fn init() {
    unsafe {
        debug!(
            "[heap_allocator::init] heap vaddr={:#x}, size={:#x}",
            addr_of_mut!(KERNEL_HEAP_MEMORY) as usize,
            config::MAX_HEAP_SIZE
        );
        HEAP_ALLOCATOR.lock().init(
            addr_of_mut!(KERNEL_HEAP_MEMORY) as usize,
            config::MAX_HEAP_SIZE,
        );
    }
}
