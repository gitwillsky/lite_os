use alloc::sync::Arc;

use crate::{
    fallible_tree::FallibleMap,
    memory::{ReclaimRequest, ReclaimResult},
};

use super::CachedPage;

pub(super) struct CachedPages {
    pub(super) entries: FallibleMap<u64, Arc<CachedPage>>,
    // OWNER: cursor 与 entries 由同一 pages lock 拥有，只表示下次 direct
    // reclaim 的起始 page index。缺失它会让脏页或外部引用前缀在每次
    // OOM 时被重复扫描，并长期饿死后续 clean page。
    reclaim_cursor: u64,
}

impl CachedPages {
    pub(super) fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
            reclaim_cursor: 0,
        }
    }

    pub(super) fn reclaim(&mut self, request: ReclaimRequest) -> ReclaimResult {
        if request.target_pages() == 0 || request.scan_pages() == 0 || self.entries.is_empty() {
            return ReclaimResult::default();
        }
        let scan_limit = request.scan_pages().min(self.entries.len());
        let mut reclaimed = 0;
        let mut scanned = 0;
        while reclaimed < request.target_pages() && scanned < scan_limit {
            // 1. 每次从持久 cursor 定位下一个 entry；到 map 末尾只回绕一次。
            let next = self
                .entries
                .iter_from(&self.reclaim_cursor)
                .next()
                .map(|(&index, page)| (index, page.reclaimable() && Arc::strong_count(page) == 1));
            let Some((index, reclaimable)) = next else {
                self.reclaim_cursor = 0;
                continue;
            };
            self.reclaim_cursor = index.checked_add(1).unwrap_or(0);
            scanned += 1;

            // 2. pages lock 使 strong_count==1 与 remove 之间不会发布新 cache clone；
            // dirty/writable 或已被 mapping 引用的页只推进 cursor，不丢失数据。
            if reclaimable {
                let removed = self.entries.remove(&index);
                debug_assert!(removed.is_some());
                reclaimed += 1;
            }
        }
        ReclaimResult::new(reclaimed, scanned)
    }
}
