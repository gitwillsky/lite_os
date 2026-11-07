//! 宿主 [`Backing`]：用标准库零初始化内存实现，供单元测试与基准使用（目标构建不编译）。
extern crate std;

use crate::{Backing, LocalHeap};
use std::alloc::{Layout, alloc_zeroed};
use std::boxed::Box;
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

/// 记录 decommit 与大对象映射次数的宿主内存。
pub struct HostBacking {
    /// `decommit` 调用次数。
    pub decommits: AtomicUsize,
    /// 当前存活的独立大对象映射数。
    pub large: AtomicUsize,
}

impl HostBacking {
    /// 计数清零的宿主内存。
    pub const fn new() -> Self {
        Self {
            decommits: AtomicUsize::new(0),
            large: AtomicUsize::new(0),
        }
    }
}

impl Default for HostBacking {
    fn default() -> Self {
        Self::new()
    }
}

std::thread_local! {
    static SLOT: Cell<Option<&'static AtomicPtr<LocalHeap>>> = const { Cell::new(None) };
}

/// 当前宿主线程的堆槽（首次使用时分配并泄漏，等价于线程描述符里的字段）。
pub fn thread_slot() -> &'static AtomicPtr<LocalHeap> {
    SLOT.with(|cell| match cell.get() {
        Some(slot) => slot,
        None => {
            let slot: &'static AtomicPtr<LocalHeap> =
                Box::leak(Box::new(AtomicPtr::new(core::ptr::null_mut())));
            cell.set(Some(slot));
            slot
        }
    })
}

// SAFETY: 区域来自标准库的零初始化分配且永不释放；其余方法只做计数或让出。
unsafe impl Backing for HostBacking {
    fn reserve(&self, size: usize, align: usize) -> Option<(usize, usize)> {
        let layout = Layout::from_size_align(size, align).ok()?;
        // SAFETY: 布局非零。
        let pointer = unsafe { alloc_zeroed(layout) };
        (!pointer.is_null()).then_some((pointer as usize, 0))
    }
    fn decommit(&self, _: usize, _: usize, _: usize) {
        self.decommits.fetch_add(1, Ordering::Relaxed);
    }
    fn map_large(&self, size: usize, align: usize) -> Option<usize> {
        self.large.fetch_add(1, Ordering::Relaxed);
        let layout = Layout::from_size_align(size.max(1), align).ok()?;
        // SAFETY: 布局非零。
        let pointer = unsafe { alloc_zeroed(layout) };
        (!pointer.is_null()).then_some(pointer as usize)
    }
    fn unmap_large(&self, _: usize, _: usize) {
        self.large.fetch_sub(1, Ordering::Relaxed);
    }
    fn thread_slot(&self) -> Option<&AtomicPtr<LocalHeap>> {
        Some(thread_slot())
    }
    fn wait(&self, _: &AtomicU32, _: u32) {
        std::thread::yield_now();
    }
    fn wake(&self, _: &AtomicU32) {}
}
