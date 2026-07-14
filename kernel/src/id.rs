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
    initial: usize,
    current: usize,
    recycled: Vec<usize>,
}

impl IdAllocator {
    pub(crate) fn new(initial_id: usize) -> Self {
        Self {
            initial: initial_id,
            current: initial_id,
            recycled: Vec::new(),
        }
    }

    /// @description 分配 ID，并同时为它未来的析构回收预留空间。
    /// @return 成功返回唯一 ID；heap 无法预留回收槽位时返回错误。
    pub(crate) fn alloc(&mut self) -> Result<usize, ()> {
        if let Some(id) = self.recycled.pop() {
            Ok(id)
        } else {
            // 每个历史签发的新 ID 都可能同时回收，因此 capacity 必须覆盖全部 fresh ID，
            // 而不是只覆盖当前 recycled.len() + 1；后者会在连续创建后批量析构时溢出。
            let issued_after = self
                .current
                .checked_sub(self.initial)
                .and_then(|issued| issued.checked_add(1))
                .ok_or(())?;
            let additional = issued_after.saturating_sub(self.recycled.len());
            self.recycled.try_reserve(additional).map_err(|_| ())?;
            let id = self.current;
            self.current = self.current.checked_add(1).ok_or(())?;
            Ok(id)
        }
    }

    pub(crate) fn dealloc(&mut self, id: usize) {
        assert!((self.initial..self.current).contains(&id));
        assert!(
            !self.recycled.contains(&id),
            "id {id} is already deallocated"
        );
        assert!(
            self.recycled.len() < self.recycled.capacity(),
            "recycle capacity proof was violated"
        );
        self.recycled.push(id);
    }
}
