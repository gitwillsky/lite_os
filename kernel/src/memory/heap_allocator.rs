use core::{alloc::{self, GlobalAlloc, Layout}, ptr::{addr_of_mut, NonNull}};

use buddy_system_allocator::LockedHeap;

use super::{config, slab_allocator::SLAB_ALLOCATOR};

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

static mut KERNEL_HEAP_MEMORY: [u8; config::MAX_HEAP_SIZE] = [0; config::MAX_HEAP_SIZE];

static BUDDY_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

/// Hybrid allocator that uses SLAB for small objects and buddy allocator for large ones
pub struct HybridAllocator;

unsafe impl GlobalAlloc for HybridAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Use SLAB allocator for small objects (<=2KB)
        if layout.size() <= 2048 {
            match SLAB_ALLOCATOR.alloc(layout) {
                Ok(ptr) => ptr.as_ptr(),
                Err(_) => {
                    // Fall back to buddy allocator if SLAB fails
                    unsafe { BUDDY_ALLOCATOR.alloc(layout) }
                }
            }
        } else {
            // Use buddy allocator for large objects
            unsafe { BUDDY_ALLOCATOR.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        // Determine which allocator owns this pointer by address range
        if Self::is_buddy_ptr(ptr) {
            // Pointer is in buddy allocator's memory range
            unsafe { BUDDY_ALLOCATOR.dealloc(ptr, layout); }
        } else {
            // Try SLAB allocator for other addresses
            if let Some(non_null_ptr) = NonNull::new(ptr) {
                if SLAB_ALLOCATOR.dealloc(non_null_ptr, layout).is_err() {
                    // This shouldn't happen - pointer doesn't belong to either allocator
                    panic!("Invalid pointer in dealloc: {:p}", ptr);
                }
            }
        }
    }
}

impl HybridAllocator {
    /// Check if a pointer belongs to the buddy allocator's memory range
    fn is_buddy_ptr(ptr: *mut u8) -> bool {
        let addr = ptr as usize;
        let heap_start = unsafe { addr_of_mut!(KERNEL_HEAP_MEMORY) as usize };
        let heap_end = heap_start + config::MAX_HEAP_SIZE;
        
        addr >= heap_start && addr < heap_end
    }
}

#[global_allocator]
static HEAP_ALLOCATOR: HybridAllocator = HybridAllocator;

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
    debug!("[heap_allocator::init] Buddy allocator initialized");
}

pub fn init_slab() {
    // Initialize SLAB allocator after frame allocator is ready
    SLAB_ALLOCATOR.init();
    debug!("[heap_allocator::init_slab] SLAB allocator initialized");
}
