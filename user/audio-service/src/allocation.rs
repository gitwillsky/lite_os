use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    // Only the mixer thread enables this flag after all of its owned state is
    // built. Without a thread-local owner, control-thread allocations would
    // make the real-time gate report false positives.
    static TRACK_MIXER: Cell<bool> = const { Cell::new(false) };
    static MIXER_ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct TrackingAllocator;

// SAFETY: Every operation delegates the exact pointer/layout contract to
// `System`. The added const-initialized TLS counters neither retain nor alter
// allocation pointers.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegates the caller's valid GlobalAlloc layout unchanged.
        let pointer = unsafe { System.alloc(layout) };
        count();
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The pointer/layout came from this allocator's System delegate.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegates the caller's valid GlobalAlloc layout unchanged.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        count();
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: Delegates the live System pointer and its original layout.
        let resized = unsafe { System.realloc(pointer, layout, size) };
        count();
        resized
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn count() {
    TRACK_MIXER.with(|tracking| {
        if tracking.get() {
            MIXER_ALLOCATIONS.with(|count| count.set(count.get().saturating_add(1)));
        }
    });
}

/// Starts cumulative allocation diagnostics on the current mixer thread.
pub(crate) fn begin_mixer_tracking() {
    MIXER_ALLOCATIONS.with(|count| count.set(0));
    TRACK_MIXER.with(|tracking| tracking.set(true));
}

/// Returns mixer allocations observed since tracking began.
pub(crate) fn mixer_allocations() -> u64 {
    MIXER_ALLOCATIONS.with(Cell::get)
}

/// Clears warm-up/lifecycle allocations at the exact physical start boundary.
pub(crate) fn reset_mixer_tracking() {
    MIXER_ALLOCATIONS.with(|count| count.set(0));
}
