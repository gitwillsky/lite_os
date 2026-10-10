//! Host allocation failures execute the production owner; no copied publication/cleanup policy.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::atomic::AtomicUsize,
};

pub(crate) static LIVE_FRAMES: AtomicUsize = AtomicUsize::new(0);
thread_local! { static FAIL_NTH: Cell<usize> = const { Cell::new(0) }; }

struct Allocator;
// SAFETY: successful allocations/deallocations delegate identical layouts to System; a failed
// allocation returns null before publication. Only the current test thread's requested call fails.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NTH
            .try_with(|counter| {
                let remaining = counter.get();
                if remaining == 0 {
                    return false;
                }
                counter.set(remaining - 1);
                remaining == 1
            })
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

pub(crate) struct FailNth;
impl FailNth {
    pub(crate) fn new(n: usize) -> Self {
        FAIL_NTH.with(|counter| counter.set(n));
        Self
    }
}
impl Drop for FailNth {
    fn drop(&mut self) {
        FAIL_NTH.with(|counter| counter.set(0));
    }
}

/// The weak Page registry is process-global; isolate scale/failure evidence from other test cases.
pub(crate) fn isolated(name: &str) -> bool {
    if std::env::var_os("LITEOS_SWAP_PAGE_ISOLATED").is_some() {
        return false;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1"])
        .env("LITEOS_SWAP_PAGE_ISOLATED", "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "isolated production-page regression failed"
    );
    true
}
