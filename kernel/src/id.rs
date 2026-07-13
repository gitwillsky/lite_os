use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

// OWNER: id module 唯一分配 kernel runtime object identity；若各 backend 独立计数，
// `/proc/<pid>/fd` 的 anonymous inode labels 会在不同对象类型间发生 identity collision。
static NEXT_RUNTIME_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

/// @description 分配一个本次 boot 内不复用的 kernel object identity。
/// @return 非零 identity；仅用于对象命名，不承担内存发布同步。
pub(crate) fn next_runtime_object_id() -> u64 {
    NEXT_RUNTIME_OBJECT_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("runtime object identity exhausted")
}

pub(crate) struct IdAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl IdAllocator {
    pub(crate) fn new(initial_id: usize) -> Self {
        Self {
            current: initial_id,
            recycled: Vec::new(),
        }
    }

    pub(crate) fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            let id = self.current;
            self.current += 1;
            id
        }
    }

    pub(crate) fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.contains(&id),
            "id {id} is already deallocated"
        );
        self.recycled.push(id);
    }
}
