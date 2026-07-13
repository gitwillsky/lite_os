use core::{
    alloc::{self, GlobalAlloc, Layout},
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use buddy_system_allocator::LockedHeap;

use super::config;
use crate::sync::LocalIrqGuard;

#[cfg(target_pointer_width = "32")]
type LockedHeapAllocator = LockedHeap<32>;

#[cfg(target_pointer_width = "64")]
type LockedHeapAllocator = LockedHeap<64>;

struct HeapStorage(UnsafeCell<[u8; config::BOOTSTRAP_HEAP_SIZE]>);

// SAFETY: heap storage is handed exactly once to the buddy allocator during single-hart init;
// every later access is serialized by the allocator lock and never uses the UnsafeCell directly.
unsafe impl Sync for HeapStorage {}

// OWNER: heap allocator exclusively owns the backing bytes after init.
static KERNEL_HEAP_MEMORY: HeapStorage =
    HeapStorage(UnsafeCell::new([0; config::BOOTSTRAP_HEAP_SIZE]));

// OWNER: buddy allocator is the only allocator metadata authority.
static BUDDY_ALLOCATOR: LockedHeapAllocator = LockedHeap::empty();

// OWNER: heap allocator 独占从 bootstrap arena 切换到 frame-backed growth 的发布状态。
// 缺失该状态会在 frame allocator 初始化前递归请求物理页，或初始化后永久停留在固定容量。
static FRAME_BACKED_GROWTH: AtomicBool = AtomicBool::new(false);

const MIN_GROWTH_PAGES: usize = 64;

fn grow(layout: Layout) -> bool {
    if !FRAME_BACKED_GROWTH.load(Ordering::Acquire) {
        return false;
    }
    let Some(required_pages) = layout
        .size()
        .max(layout.align())
        .div_ceil(config::PAGE_SIZE)
        .checked_next_power_of_two()
    else {
        return false;
    };
    let desired_pages = required_pages.max(MIN_GROWTH_PAGES);
    let frames = super::frame_allocator::alloc_contiguous(desired_pages).or_else(|| {
        (required_pages != desired_pages)
            .then(|| super::frame_allocator::alloc_contiguous(required_pages))
            .flatten()
    });
    let Some(frames) = frames else {
        return false;
    };
    let start = frames.ppn.as_usize() * config::PAGE_SIZE;
    let end = start + frames.pages * config::PAGE_SIZE;
    // SAFETY: FrameTracker 独占连续、页对齐且由 kernel physmap 永久覆盖的物理内存；
    // forget 将其所有权转移给 buddy allocator，之后只能通过 GlobalAlloc 分配/释放子块。
    unsafe { BUDDY_ALLOCATOR.lock().add_to_heap(start, end) };
    core::mem::forget(frames);
    true
}

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
        // 1. 快路径只获取 buddy lock；失败后必须先释放它再取 frame lock，固定 lock ordering。
        if let Ok(allocation) = BUDDY_ALLOCATOR.lock().alloc(layout) {
            return allocation.as_ptr();
        }
        // 2. 从唯一 frame allocator 转移一段连续物理页，再重试原布局；不设置容量上限。
        if grow(layout)
            && let Ok(allocation) = BUDDY_ALLOCATOR.lock().alloc(layout)
        {
            return allocation.as_ptr();
        }
        core::ptr::null_mut()
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

        // SAFETY: GlobalAlloc caller 保证 ptr 来自同一 buddy allocator 且 layout 完全一致。
        unsafe {
            BUDDY_ALLOCATOR
                .lock()
                .dealloc(NonNull::new_unchecked(ptr), layout)
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
            config::BOOTSTRAP_HEAP_SIZE,
        );
    }
}

/// @description 在 frame allocator 初始化后启用无固定上限的物理页扩容。
/// @return 无返回值；重复调用保持启用状态。
pub(crate) fn enable_frame_backed_growth() {
    FRAME_BACKED_GROWTH.store(true, Ordering::Release);
}
