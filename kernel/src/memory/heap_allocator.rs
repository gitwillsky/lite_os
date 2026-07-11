use core::{
    alloc::{self, GlobalAlloc, Layout},
    ptr::addr_of_mut,
};

use buddy_system_allocator::LockedHeap;

use super::config;
use crate::sync::LocalIrqGuard;

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

static mut KERNEL_HEAP_MEMORY: [u8; config::MAX_HEAP_SIZE] = [0; config::MAX_HEAP_SIZE];

static BUDDY_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Per Rust GlobalAlloc contract: size==0 may return any non-null, well-aligned pointer
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        // timer softirq 当前仍会分配；包围整个 allocator 路径可防止同 hart 中断重入内部 spin lock。
        let _irq = LocalIrqGuard::disable();
        unsafe { BUDDY_ALLOCATOR.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Per Rust GlobalAlloc contract: size==0 dealloc is a no-op; ptr may be dangling
        if layout.size() == 0 {
            return;
        }

        if ptr.is_null() {
            return;
        }
        // 与 alloc 使用同一 IRQ 约束；缺失时 interrupt dealloc/alloc 可重入 buddy lock。
        let _irq = LocalIrqGuard::disable();

        unsafe {
            BUDDY_ALLOCATOR.dealloc(ptr, layout);
        }
    }
}

#[global_allocator]
static HEAP_ALLOCATOR: KernelAllocator = KernelAllocator;

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
        BUDDY_ALLOCATOR.lock().init(
            addr_of_mut!(KERNEL_HEAP_MEMORY) as usize,
            config::MAX_HEAP_SIZE,
        );
    }
    debug!("[heap_allocator::init] allocator initialized");
}
