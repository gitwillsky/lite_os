use super::*;
use crate::memory::page_table::PageTableError;

impl MemorySet {
    pub(super) fn allocate_private_frame(&mut self) -> Result<FrameTracker, MemoryError> {
        if let Some(frame) = alloc() {
            return Ok(frame);
        }
        // 1. 当前 mm 在已持有 AddressSpace lock 下直接回收；registry 会通过 try_lock 跳过它。
        let _ = self.reclaim_private_pages(ReclaimRequest::for_target(64));
        // 2. alloc 的统一慢路径会在需要时再请求其他 resident owner，最后只重试一次。
        alloc().ok_or(MemoryError::OutOfMemory)
    }

    pub(super) fn next_private_resident_from(
        &self,
        cursor: VirtualPageNumber,
    ) -> Option<(VirtualPageNumber, VirtualPageNumber)> {
        // 1. cursor 可能落在一个更早起始的 VMA 中；先查该 area 的后缀。
        let previous = self.areas.floor(&cursor);
        if let Some((&key, area)) = previous
            && cursor < area.vpn_range.end
            && let Some((&vpn, _)) = area.data_frames.ceiling(&cursor)
        {
            return Some((key, vpn));
        }

        // 2. 从 predecessor 之后的 VMA 开始，每个 area 只查最小 resident VPN。
        // 不构造 key Vec，避免 OOM 路径递归进入 global allocator。
        let mut remaining = match previous {
            Some((&key, _)) => self.areas.iter_after(&key),
            None => self.areas.iter_from(&cursor),
        };
        remaining.find_map(|(&key, area)| {
            area.data_frames
                .first_key_value()
                .map(|(&vpn, _)| (key, vpn))
        })
    }

    /// 有界回收 MADV_FREE 页与可从 immutable backing 重建的 clean private 页。
    ///
    /// # Parameters
    ///
    /// - `request`: 需要释放的物理页目标与 resident entry 扫描上限。
    ///
    /// # Returns
    ///
    /// 实际释放和扫描的页数；共享 COW frame 只撤销本 mm 映射，不伪报释放。
    pub(crate) fn reclaim_private_pages(&mut self, request: ReclaimRequest) -> ReclaimResult {
        if request.target_pages() == 0 || request.scan_pages() == 0 {
            return ReclaimResult::default();
        }
        self.reclaim_clean_private_pages(request)
    }

    /// 同一 clean retirement transaction 可处理整批 direct reclaim 或统一后台扫描的一个候选。
    pub(super) fn reclaim_clean_private_pages(&mut self, request: ReclaimRequest) -> ReclaimResult {
        let transaction = PrivateReclaimTransaction::new(self, request);
        let transaction = revoke_and_synchronize(
            transaction,
            PrivateReclaimTransaction::revoke,
            PrivateReclaimTransaction::synchronize,
        )
        .expect("platform TLB synchronization failed after private page reclaim");
        transaction.release()
    }
}

/// 一次不分配内存的 private-resident retire transaction。
struct PrivateReclaimTransaction<'memory> {
    memory: &'memory mut MemorySet,
    request: ReclaimRequest,
    initial_cursor: VirtualPageNumber,
    final_cursor: VirtualPageNumber,
    scanned: usize,
    revoke_unique_candidates: usize,
    commit: TranslationCommit,
}

impl<'memory> PrivateReclaimTransaction<'memory> {
    fn new(memory: &'memory mut MemorySet, request: ReclaimRequest) -> Self {
        let initial_cursor = memory.private_reclaim_cursor;
        Self {
            memory,
            request,
            initial_cursor,
            final_cursor: initial_cursor,
            scanned: 0,
            revoke_unique_candidates: 0,
            commit: TranslationCommit::new(),
        }
    }

    fn revoke(&mut self) {
        let mut walk = PrivateReclaimWalk::new(self.memory.private_reclaim_cursor.as_usize());
        while self.revoke_unique_candidates < self.request.target_pages()
            && self.scanned < self.request.scan_pages()
        {
            // resident owner 此阶段保持原位，保证 fence 后可按同一 deterministic sequence
            // 重放并释放；walk 只在实际扫描页后推进 cursor，wrap 不提交 cursor。
            let next = self
                .memory
                .next_private_resident_from(VirtualPageNumber::from_vpn(walk.probe()));
            let Some((area_key, vpn)) = next else {
                if !walk.wrap_or_finish() {
                    break;
                }
                continue;
            };
            if !walk.advance(vpn.as_usize()) {
                break;
            }
            self.scanned += 1;
            if self
                .memory
                .private_reclaim_exclusion
                .as_ref()
                .is_some_and(|range| range.contains(&vpn))
            {
                continue;
            }

            let area = self
                .memory
                .areas
                .get_mut(&area_key)
                .expect("private reclaim lost resident VMA");
            let resident = area
                .data_frames
                .get(&vpn)
                .expect("private reclaim lost resident page");
            let reclaimable =
                resident.discardable || area.private_file.is_some() && !resident.dirty;
            if !reclaimable {
                continue;
            }
            // 这里只用 revoke-time count 控制扫描节奏；其他 mm 可在 fence 期间释放同一
            // COW Arc，因此结果必须在 release replay 重新判定，不能把此值当作不变量。
            self.revoke_unique_candidates += usize::from(resident.exclusive());
            match self.memory.page_table.unmap(vpn, &mut self.commit) {
                Ok(()) => {}
                Err(PageTableError::NotMapped) => {}
                Err(error) => panic!("private reclaim failed to unmap {vpn:?}: {error:?}"),
            }
        }
        self.memory.private_reclaim_cursor = VirtualPageNumber::from_vpn(walk.committed());
        self.final_cursor = self.memory.private_reclaim_cursor;
    }

    fn synchronize(&mut self) -> Result<(), TranslationSynchronizationError> {
        self.commit.synchronize()
    }

    fn release(self) -> ReclaimResult {
        let mut walk = PrivateReclaimWalk::new(self.initial_cursor.as_usize());
        let mut replayed = 0;
        let mut reclaimed = 0;
        while replayed < self.scanned {
            let next = self
                .memory
                .next_private_resident_from(VirtualPageNumber::from_vpn(walk.probe()));
            let Some((area_key, vpn)) = next else {
                assert!(
                    walk.wrap_or_finish(),
                    "private reclaim replay ended before scan budget"
                );
                continue;
            };
            assert!(
                walk.advance(vpn.as_usize()),
                "private reclaim replay crossed its initial cursor"
            );
            replayed += 1;
            if self
                .memory
                .private_reclaim_exclusion
                .as_ref()
                .is_some_and(|range| range.contains(&vpn))
            {
                continue;
            }

            let area = self
                .memory
                .areas
                .get_mut(&area_key)
                .expect("private reclaim replay lost resident VMA");
            let resident = area
                .data_frames
                .get(&vpn)
                .expect("private reclaim replay lost resident page");
            let reclaimable =
                resident.discardable || area.private_file.is_some() && !resident.dirty;
            if !reclaimable {
                continue;
            }
            let decision = reclaim_release_decision(
                reclaimed,
                self.request.target_pages(),
                if resident.exclusive() { 1 } else { 2 },
            );
            if !decision.release {
                // PTE 已在 revoke 阶段撤销；保留 resident owner 后，后续 fault 可直接重建
                // translation，同时保证 adapter result 不超过 caller target。
                continue;
            }
            let removed = area
                .data_frames
                .remove(&vpn)
                .expect("private reclaim replay lost selected page");
            reclaimed += usize::from(removed.release_for_reclaim());
        }
        if self.scanned != 0 {
            assert_eq!(
                walk.committed(),
                self.final_cursor.as_usize(),
                "private reclaim replay diverged"
            );
        }
        ReclaimResult::new(reclaimed, replayed)
    }
}
