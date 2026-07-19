/// Deferred final-cleanup 的固定容量 FIFO owner。
///
/// 容量与 active SocketSet slot 上限相同：poll 持有 stack owner 时不能创建新 endpoint，
/// 每个 live `InetSocket` 至少占一个尚未复用的 slot，且 final Drop 只发布一次 identity。
/// 因此 publish 无需 O(N) duplicate scan，单轮最多积累 `N` 个 cleanup，也不需要 fallback queue。
pub(super) struct PendingCleanup<T: Copy, const N: usize> {
    slots: [Option<T>; N],
    head: usize,
    tail: usize,
    length: usize,
}

/// Fixed ring 已达到与 SocketSet 相同的容量上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Full;

impl<T: Copy, const N: usize> PendingCleanup<T, N> {
    /// @description 创建空的固定容量 cleanup ring。
    /// @param N 与生产 SocketSet capacity 相同的 compile-time slot 数。
    /// @return head/tail/length 全零的 ring。
    /// @errors N 为零时 const invariant fail-stop。
    pub(super) const fn new() -> Self {
        assert!(N != 0, "pending cleanup ring requires non-zero capacity");
        Self {
            slots: [const { None }; N],
            head: 0,
            tail: 0,
            length: 0,
        }
    }

    /// @description O(1) 发布一个 exactly-once final-drop identity。
    /// @param value 仍占用唯一 endpoint slot、因此尚不可能被复用的 identity。
    /// @return 成功时返回 unit。
    /// @errors 达到 active SocketSet slot 上限时返回 `Full`，表示 owner 不变量被破坏。
    pub(super) fn publish(&mut self, value: T) -> Result<(), Full> {
        if self.length == N {
            return Err(Full);
        }
        debug_assert!(self.slots[self.tail].is_none());
        self.slots[self.tail] = Some(value);
        self.tail = if self.tail + 1 == N { 0 } else { self.tail + 1 };
        self.length += 1;
        Ok(())
    }

    /// @description O(1) 摘取最早发布的 cleanup identity。
    /// @return 空 ring 返回 `None`，否则返回唯一 identity 并立即释放对应 ring slot。
    /// @errors length 与 slot publication 分裂时 fail-stop，不提供损坏状态 fallback。
    pub(super) fn pop(&mut self) -> Option<T> {
        if self.length == 0 {
            return None;
        }
        let value = self.slots[self.head]
            .take()
            .expect("pending cleanup length diverged from ring slot");
        self.head = if self.head + 1 == N { 0 } else { self.head + 1 };
        self.length -= 1;
        Some(value)
    }

    /// @description 判断 fixed-budget drain 后是否仍需回投 deferred work。
    /// @return ring 非空时为 true。
    /// @errors 无错误。
    pub(super) const fn has_pending(&self) -> bool {
        self.length != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{Full, PendingCleanup};
    use std::sync::{Arc, Mutex};

    #[test]
    fn capacity_boundary_is_explicit() {
        let mut pending = PendingCleanup::<usize, 4>::new();
        for identity in 0..4 {
            pending.publish(identity).unwrap();
        }
        assert_eq!(pending.publish(4), Err(Full));
        assert_eq!(
            core::array::from_fn::<_, 4, _>(|_| pending.pop().unwrap()),
            [0, 1, 2, 3]
        );
        assert!(!pending.has_pending());
    }

    #[test]
    fn wraparound_preserves_fifo_order() {
        let mut pending = PendingCleanup::<usize, 4>::new();
        for identity in 0..4 {
            pending.publish(identity).unwrap();
        }
        assert_eq!(pending.pop(), Some(0));
        assert_eq!(pending.pop(), Some(1));
        pending.publish(4).unwrap();
        pending.publish(5).unwrap();
        assert_eq!(
            core::array::from_fn::<_, 4, _>(|_| pending.pop().unwrap()),
            [2, 3, 4, 5]
        );
    }

    #[test]
    fn concurrent_final_drops_fill_each_slot_once() {
        const CAPACITY: usize = 64;
        let pending = Arc::new(Mutex::new(PendingCleanup::<usize, CAPACITY>::new()));
        std::thread::scope(|scope| {
            for identity in 0..CAPACITY {
                let pending = pending.clone();
                scope.spawn(move || pending.lock().unwrap().publish(identity).unwrap());
            }
        });
        let mut identities =
            core::array::from_fn::<_, CAPACITY, _>(|_| pending.lock().unwrap().pop().unwrap());
        identities.sort_unstable();
        assert_eq!(identities, core::array::from_fn(|index| index));
    }

    #[test]
    fn fixed_budget_drain_reports_backlog() {
        const CAPACITY: usize = 8;
        let mut pending = PendingCleanup::<usize, CAPACITY>::new();
        for identity in 0..CAPACITY {
            pending.publish(identity).unwrap();
        }
        let drained = core::array::from_fn::<_, 3, _>(|_| pending.pop().unwrap());
        assert_eq!(drained, [0, 1, 2]);
        assert!(pending.has_pending());
    }
}
