//! futex 等待队列的纯逻辑：key、哈希分桶、FIFO 唤醒与跨桶 requeue。
//!
//! 不含锁、线程与用户内存：桶锁、用户值比较和睡眠在上层；这里只在“已持有桶锁”的前提下改写表，
//! 因此可以脱离内核做宿主单测与基准。

use fallible_tree::{FallibleMap, VacantEntry};

/// 一个 futex 的身份：私有按（空间, 虚拟地址），共享按（VMO, 字节偏移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FutexKey {
    Private { space: u64, address: usize },
    Shared { vmo: u64, offset: u64 },
}

impl FutexKey {
    /// 桶下标：两个 64 位部分经黄金比例乘法混合；相同 key 恒落在同一个桶。
    ///
    /// `buckets` 必须是不小于 2 的 2 的幂。
    pub(crate) fn bucket(self, buckets: usize) -> usize {
        const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
        debug_assert!(buckets.is_power_of_two() && buckets >= 2);
        let (tag, high, low) = match self {
            Self::Private { space, address } => (1u64, space, address as u64),
            Self::Shared { vmo, offset } => (2u64, vmo, offset),
        };
        let hash = (high.wrapping_mul(MIX) ^ low.wrapping_add(tag))
            .wrapping_mul(MIX)
            .rotate_left(29)
            .wrapping_mul(MIX);
        (hash >> (u64::BITS - buckets.trailing_zeros())) as usize
    }
}

/// 等待者在桶内的位置：（key, 登记序号）。序号单调递增，同 key 内先入先出。
pub(crate) type Slot = (FutexKey, u64);

/// 一个桶：等待者按 [`Slot`] 排序。
pub(crate) struct Bucket<W> {
    waiters: FallibleMap<Slot, W>,
}

impl<W> Bucket<W> {
    /// 空桶。
    pub(crate) const fn new() -> Self {
        Self {
            waiters: FallibleMap::new(),
        }
    }

    /// 为 `slot` 预分配节点（可失败的一步放在拿桶锁之前）。
    ///
    /// # Errors
    /// 分配失败返回 `Err(())`。
    pub(crate) fn prepare(slot: Slot, waiter: W) -> Result<VacantEntry<Slot, W>, ()> {
        FallibleMap::try_prepare(slot, waiter).map_err(|_| ())
    }

    /// 入队预分配的节点；不分配、不失败。
    pub(crate) fn enqueue(&mut self, entry: VacantEntry<Slot, W>) {
        self.waiters.commit_vacant(entry);
    }

    /// 摘除 `slot` 处的等待者（已被唤醒者不在桶里）。
    pub(crate) fn remove(&mut self, slot: &Slot) -> Option<W> {
        self.waiters.remove(slot)
    }

    /// 桶内等待者数量。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.waiters.len()
    }

    /// `key` 的最早登记位置。
    fn first(&self, key: FutexKey) -> Option<Slot> {
        self.waiters
            .ceiling(&(key, 0))
            .map(|(slot, _)| *slot)
            .filter(|slot| slot.0 == key)
    }
}

/// 摘除并唤醒至多 `count` 个 `key` 的等待者，逐个交给 `wake`；返回实际唤醒数。
pub(crate) fn wake_locked<W>(
    table: &mut Bucket<W>,
    key: FutexKey,
    count: usize,
    mut wake: impl FnMut(W),
) -> u32 {
    let mut woken = 0;
    while (woken as usize) < count {
        let Some(slot) = table.first(key) else { break };
        wake(table.remove(&slot).expect("futex waiter vanished"));
        woken += 1;
    }
    woken
}

/// 一次 requeue 的参数。
pub(crate) struct Requeue {
    pub(crate) source: FutexKey,
    pub(crate) target: FutexKey,
    /// 先唤醒的最大数量。
    pub(crate) wake: usize,
    /// 再移动到 `target` 的最大数量。
    pub(crate) requeue: usize,
}

/// requeue 对等待者记录的三个回调。
pub(crate) trait RequeueHooks<W> {
    /// 被唤醒的等待者。
    fn wake(&mut self, waiter: W);
    /// 为移动后的新位置分配登记序号；保证它排在 `target` 现有等待者之后。
    fn next_ticket(&mut self) -> u64;
    /// 等待者被移到新登记位置；调用者据此改写它记录的位置。
    fn moved(&mut self, waiter: &W, position: (FutexKey, u64));
}

/// 先唤醒至多 `plan.wake` 个 `source` 的等待者，再把至多 `plan.requeue` 个移到 `target`。
///
/// `target_table` 为 `None` 表示 `target` 与 `source` 在同一个桶。返回被移动的数量。
pub(crate) fn requeue_locked<W>(
    source_table: &mut Bucket<W>,
    mut target_table: Option<&mut Bucket<W>>,
    plan: &Requeue,
    hooks: &mut impl RequeueHooks<W>,
) -> usize {
    wake_locked(source_table, plan.source, plan.wake, |waiter| {
        hooks.wake(waiter)
    });
    let mut count = 0;
    while count < plan.requeue {
        let Some(slot) = source_table.first(plan.source) else {
            break;
        };
        let mut entry = source_table
            .waiters
            .take_entry(&slot)
            .expect("futex waiter vanished");
        let position = (plan.target, hooks.next_ticket());
        hooks.moved(entry.value(), position);
        entry.set_key(position);
        match target_table.as_deref_mut() {
            Some(table) => table.enqueue(entry),
            None => source_table.enqueue(entry),
        }
        count += 1;
    }
    count
}
