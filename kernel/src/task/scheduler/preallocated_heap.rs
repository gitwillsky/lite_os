use alloc::collections::BinaryHeap;

/// @description 启动期预留完整 backing、运行期只做原地 heap mutation 的有界容器。
///
/// `make_room` 把昂贵的 retain 隐藏在 capacity-pressure seam 后；只要已有空间，
/// predicate 不会被调用。`discard_invalid_roots` 只维护调度可见的 heap minimum，
/// 不把普通 push 退化为全容器 validation。
pub(super) struct PreallocatedHeap<T: Ord> {
    entries: BinaryHeap<T>,
}

impl<T: Ord> PreallocatedHeap<T> {
    /// @description 构造运行期无需扩容的空 heap。
    /// @param capacity 运行期最大物理 entry 数。
    /// @return 成功返回完整预留的 heap；allocator OOM 返回错误。
    pub(super) fn try_with_capacity(capacity: usize) -> Result<Self, ()> {
        let mut entries = BinaryHeap::new();
        entries.try_reserve_exact(capacity).map_err(|_| ())?;
        Ok(Self { entries })
    }

    /// @description 在不分配的前提下保证 `additional` 个空 slot。
    /// @param additional 本次 transaction 将插入的 entry 数。
    /// @param keep 仅在现有 spare capacity 不足时用于原地清除 stale entry。
    /// @return 本次实际清除的 entry 数；普通有空间路径固定为零。
    #[inline(always)]
    pub(super) fn make_room(&mut self, additional: usize, keep: impl FnMut(&T) -> bool) -> usize {
        assert!(additional <= self.entries.capacity());
        if additional <= self.entries.capacity() - self.entries.len() {
            return 0;
        }
        compact_for_capacity(&mut self.entries, additional, keep)
    }

    /// @description 删除连续的 invalid heap roots，保持第一个可见 root 有效。
    /// @param keep 判定 entry 是否仍可作为当前 ordered root。
    /// @return 删除的 invalid root 数量。
    #[inline(always)]
    pub(super) fn discard_invalid_roots(&mut self, mut keep: impl FnMut(&T) -> bool) -> usize {
        if self.entries.peek().is_none_or(&mut keep) {
            return 0;
        }
        discard_invalid_roots_slow(&mut self.entries, keep)
    }

    /// @description 插入一个已由 `make_room` 证明有 backing 的 entry。
    pub(super) fn push(&mut self, entry: T) {
        assert!(
            self.entries.len() < self.entries.capacity(),
            "preallocated heap push skipped capacity proof"
        );
        self.entries.push(entry);
    }

    /// @description 移除 ordered root。
    pub(super) fn pop(&mut self) -> Option<T> {
        self.entries.pop()
    }

    /// @description 借用 ordered root。
    pub(super) fn peek(&self) -> Option<&T> {
        self.entries.peek()
    }

    /// @description 返回物理 entry 数，供同一生产 seam 的 host test 验证扫描策略。
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.entries.capacity()
    }
}

#[cold]
#[inline(never)]
fn compact_for_capacity<T: Ord>(
    entries: &mut BinaryHeap<T>,
    additional: usize,
    keep: impl FnMut(&T) -> bool,
) -> usize {
    let before = entries.len();
    entries.retain(keep);
    assert!(
        additional <= entries.capacity() - entries.len(),
        "preallocated heap capacity exhausted by live entries"
    );
    before - entries.len()
}

#[cold]
#[inline(never)]
fn discard_invalid_roots_slow<T: Ord>(
    entries: &mut BinaryHeap<T>,
    mut keep: impl FnMut(&T) -> bool,
) -> usize {
    let mut removed = 1;
    entries.pop();
    while entries.peek().is_some_and(|entry| !keep(entry)) {
        entries.pop();
        removed += 1;
    }
    removed
}
