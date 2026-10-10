//! 线程局部一次 allocation fault；测试真实 fallible allocation 的 rollback。
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_AFTER: Cell<Option<usize>> = const { Cell::new(None) }; }
struct FaultAllocator;
// SAFETY: 正常 allocation/deallocation 原样委托 System；只有当前测试线程的一次申请返回 null。
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_AFTER
            .try_with(|flag| match flag.get() {
                Some(0) => {
                    flag.set(None);
                    true
                }
                Some(count) => {
                    flag.set(Some(count - 1));
                    false
                }
                None => false,
            })
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;
pub(crate) fn fail_next() {
    fail_after(0);
}
pub(crate) fn was_consumed() -> bool {
    FAIL_AFTER.with(|flag| flag.replace(None)).is_none()
}

pub(crate) fn fail_after(successful_allocations: usize) {
    FAIL_AFTER.with(|flag| flag.set(Some(successful_allocations)));
}
