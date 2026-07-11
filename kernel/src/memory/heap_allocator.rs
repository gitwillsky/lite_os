use core::{
    alloc::{self, GlobalAlloc, Layout},
    cell::UnsafeCell,
};

use buddy_system_allocator::LockedHeap;

use super::config;
use crate::sync::LocalIrqGuard;

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

struct HeapStorage(UnsafeCell<[u8; config::MAX_HEAP_SIZE]>);

// SAFETY: heap storage is handed exactly once to the buddy allocator during single-hart init;
// every later access is serialized by the allocator lock and never uses the UnsafeCell directly.
unsafe impl Sync for HeapStorage {}

// OWNER: heap allocator exclusively owns the backing bytes after init.
static KERNEL_HEAP_MEMORY: HeapStorage = HeapStorage(UnsafeCell::new([0; config::MAX_HEAP_SIZE]));

// OWNER: buddy allocator is the only allocator metadata authority.
static BUDDY_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

pub(crate) struct KernelAllocator;

// SAFETY: this adapter forwards the GlobalAlloc contract to one locked buddy allocator;
// LocalIrqGuard prevents same-hart interrupt re-entry around every allocator operation.
unsafe impl GlobalAlloc for KernelAllocator {
    // SAFETY: caller must satisfy GlobalAlloc's layout contract; the locked buddy allocator
    // returns storage from the permanently owned heap region.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Per Rust GlobalAlloc contract: size==0 may return any non-null, well-aligned pointer
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        // timer softirq 当前仍会分配；包围整个 allocator 路径可防止同 hart 中断重入内部 spin lock。
        let _irq = LocalIrqGuard::disable();
        // SAFETY: forwarded caller layout contract and permanent allocator backing satisfy
        // LockedHeap::alloc requirements.
        unsafe { BUDDY_ALLOCATOR.alloc(layout) }
    }

    // SAFETY: caller must pass a pointer/layout pair previously returned by this allocator.
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

        // SAFETY: the GlobalAlloc caller guarantees this exact pointer/layout allocation pair.
        unsafe {
            BUDDY_ALLOCATOR.dealloc(ptr, layout);
        }
    }
}

#[global_allocator]
// OWNER: Rust allocation ABI delegates exclusively to this adapter.
static HEAP_ALLOCATOR: KernelAllocator = KernelAllocator;

#[alloc_error_handler]
pub(crate) fn handle_heap_alloc_error(layout: alloc::Layout) -> ! {
    panic!("allocate heap memory error, layout = {:?}", layout);
}

pub(crate) fn init() {
    // SAFETY: boot initialization calls this once before allocation is exposed to other harts;
    // the pointer covers the complete static HeapStorage and remains valid forever.
    unsafe {
        BUDDY_ALLOCATOR.lock().init(
            KERNEL_HEAP_MEMORY.0.get().cast::<u8>() as usize,
            config::MAX_HEAP_SIZE,
        );
    }
}
