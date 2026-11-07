//! 字形缓存：LRU 淘汰，而不是满了整体清空。
//!
//! 整体清空会让正在显示的全部文字在同一帧重新光栅化（每个字形执行 hinting 指令），出现周期性卡顿；
//! LRU 只丢弃最久未用的一批，常驻界面文字不受影响。批量淘汰（一次丢弃四分之一）把淘汰的扫描成本摊销到
//! 之后的插入上，命中路径只是一次哈希查找加一次计数器写入。

use std::collections::HashMap;
use std::hash::Hash;

/// 淘汰时丢弃的比例分母：一次丢弃最久未用的 `1 / EVICT_FRACTION`。
const EVICT_FRACTION: usize = 4;

/// 容量固定的 LRU 缓存。
pub(crate) struct LruCache<K, V> {
    capacity: usize,
    entries: HashMap<K, (V, u64)>,
    // 单调递增的使用序号；u64 在任何现实负载下不会回绕。
    clock: u64,
}

impl<K: Hash + Eq + Copy, V: Clone> LruCache<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity >= EVICT_FRACTION);
        Self {
            capacity,
            entries: HashMap::new(),
            clock: 0,
        }
    }

    /// 命中时刷新使用序号并返回副本。
    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|(value, used)| {
            *used = clock;
            value.clone()
        })
    }

    /// 插入；满时先淘汰最久未用的四分之一。
    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict();
        }
        self.clock += 1;
        self.entries.insert(key, (value, self.clock));
    }

    fn evict(&mut self) {
        let mut stamps: Vec<u64> = self.entries.values().map(|(_, used)| *used).collect();
        let drop_count = (self.capacity / EVICT_FRACTION).max(1);
        // 第 drop_count 小的使用序号是淘汰线：严格小于它的丢弃，序号唯一所以恰好丢弃 drop_count 个。
        let (_, &mut threshold, _) = stamps.select_nth_unstable(drop_count - 1);
        self.entries.retain(|_, (_, used)| *used > threshold);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::LruCache;

    #[test]
    fn eviction_drops_the_least_recently_used_quarter_and_keeps_hot_entries() {
        let mut cache = LruCache::new(8);
        for key in 0..8u32 {
            cache.insert(key, key * 10);
        }
        // 0、1 保持热门；2、3 最久未用。
        assert_eq!(cache.get(&0), Some(0));
        assert_eq!(cache.get(&1), Some(10));
        cache.insert(100, 1000);
        assert_eq!(cache.len(), 7, "a quarter of eight entries is evicted, then one is added");
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), None);
        for hot in [0, 1, 4, 5, 6, 7, 100] {
            assert!(cache.get(&hot).is_some(), "{hot} must survive");
        }
    }

    #[test]
    fn replacing_an_existing_key_does_not_evict() {
        let mut cache = LruCache::new(4);
        for key in 0..4u32 {
            cache.insert(key, key);
        }
        cache.insert(2, 99);
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get(&2), Some(99));
    }
}
