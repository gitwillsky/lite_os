use alloc::sync::Arc;

use crate::{fallible_tree::FallibleMap, memory::ReclaimRequest};

use super::{CachedPage, WRITEBACK_BATCH_PAGES};

pub(super) struct PreparedReclaim {
    pub(super) writeback: [Option<(u64, Arc<CachedPage>)>; WRITEBACK_BATCH_PAGES],
    pub(super) writeback_pages: usize,
    pub(super) reclaimed_pages: usize,
    pub(super) scanned_pages: usize,
}

impl PreparedReclaim {
    fn empty() -> Self {
        Self {
            writeback: core::array::from_fn(|_| None),
            writeback_pages: 0,
            reclaimed_pages: 0,
            scanned_pages: 0,
        }
    }
}

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

    pub(super) fn prepare_reclaim(
        &mut self,
        request: ReclaimRequest,
        allow_writeback: bool,
    ) -> PreparedReclaim {
        if request.target_pages() == 0 || request.scan_pages() == 0 || self.entries.is_empty() {
            return PreparedReclaim::empty();
        }
        let scan_limit = request.scan_pages().min(self.entries.len());
        let mut prepared = PreparedReclaim::empty();
        while prepared.reclaimed_pages + prepared.writeback_pages < request.target_pages()
            && prepared.scanned_pages < scan_limit
        {
            // 1. 每次从持久 cursor 定位下一个 entry；到 map 末尾只回绕一次。
            let next = self
                .entries
                .iter_from(&self.reclaim_cursor)
                .next()
                .map(|(&index, page)| {
                    (
                        index,
                        page.reclaimable() && Arc::strong_count(page) == 1,
                        allow_writeback && page.dirty_unmapped() && Arc::strong_count(page) == 1,
                    )
                });
            let Some((index, reclaimable, writeback)) = next else {
                self.reclaim_cursor = 0;
                continue;
            };
            self.reclaim_cursor = index.checked_add(1).unwrap_or(0);
            prepared.scanned_pages += 1;

            // 2. clean 独占页立即删除；dirty 且没有 writer/外部引用的页只固定到 stack batch。
            //    提交失败时 entry 与 dirty bit 都保持不变，绝不以回收压力丢弃数据。
            if reclaimable {
                let removed = self.entries.remove(&index);
                debug_assert!(removed.is_some());
                prepared.reclaimed_pages += 1;
            } else if writeback && prepared.writeback_pages < WRITEBACK_BATCH_PAGES {
                prepared.writeback[prepared.writeback_pages] =
                    self.entries.get(&index).map(|page| (index, page.clone()));
                prepared.writeback_pages += 1;
            }
        }
        prepared
    }
}
