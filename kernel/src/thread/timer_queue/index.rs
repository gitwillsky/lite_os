//! 截止时间有序索引：按（截止时间, 序号）排序，序号保证同一截止时间下先入先出。
//!
//! 只负责排序与“队首是否变化”的判定——这是每 CPU 硬件 deadline 合成唯一需要的事实；
//! 线程、唤醒与锁都在上层。节点在锁外预分配，入队/出队不分配。

use fallible_tree::{FallibleMap, VacantEntry};

/// （截止时间 ns, 序号）。
pub(crate) type Key = (u64, u64);

/// 取消的结果。
pub(crate) struct Cancelled<V> {
    /// 被摘除的节点；锁内不得释放其值。
    pub(crate) removed: Option<VacantEntry<Key, V>>,
    /// 队首是否因此变化（调用者需要重新发布队首截止时间）。
    pub(crate) head_changed: bool,
}

/// 一次取走到期节点的结果。
pub(crate) struct Due {
    /// 本次取走的节点数；非零即队首已前进，调用者需要重新发布队首截止时间。
    pub(crate) taken: usize,
    /// 取满一批后队内仍有到期节点。
    pub(crate) more_due: bool,
}

/// 按截止时间排序的节点集合。
pub(crate) struct DeadlineIndex<V> {
    map: FallibleMap<Key, V>,
}

impl<V> DeadlineIndex<V> {
    /// 空索引。
    pub(crate) const fn new() -> Self {
        Self {
            map: FallibleMap::new(),
        }
    }

    /// 为 `key` 预分配节点。
    ///
    /// # Errors
    /// 分配失败返回 `Err(())`。
    pub(crate) fn prepare(key: Key, value: V) -> Result<VacantEntry<Key, V>, ()> {
        FallibleMap::try_prepare(key, value).map_err(|_| ())
    }

    /// 队首截止时间。
    pub(crate) fn head(&self) -> Option<u64> {
        self.map.first_key_value().map(|(key, _)| key.0)
    }

    /// 入队；返回新节点是否成为队首。不分配、不失败。
    pub(crate) fn insert(&mut self, entry: VacantEntry<Key, V>) -> bool {
        let key = *entry.key();
        self.map.commit_vacant(entry);
        self.map
            .first_key_value()
            .is_some_and(|(first, _)| *first == key)
    }

    /// 取消 `key`；已到期（不在索引中）则什么都不做。
    pub(crate) fn cancel(&mut self, key: &Key) -> Cancelled<V> {
        let was_head = self
            .map
            .first_key_value()
            .is_some_and(|(first, _)| first == key);
        let removed = self.map.take_entry(key);
        Cancelled {
            head_changed: was_head && removed.is_some(),
            removed,
        }
    }

    /// 取走截止时间不晚于 `now_ns` 的节点，填入 `batch` 的前缀。
    pub(crate) fn take_due(
        &mut self,
        now_ns: u64,
        batch: &mut [Option<VacantEntry<Key, V>>],
    ) -> Due {
        let mut taken = 0;
        for slot in batch.iter_mut() {
            let Some((&key, _)) = self.map.first_key_value() else {
                break;
            };
            if key.0 > now_ns {
                break;
            }
            *slot = self.map.take_entry(&key);
            taken += 1;
        }
        Due {
            taken,
            more_due: self.head().is_some_and(|head| head <= now_ns),
        }
    }
}
