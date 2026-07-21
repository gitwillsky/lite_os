use super::*;
use crate::memory::page_table::PageTableError;

impl MemorySet {
    /// @description 对完整用户 VMA 区间应用 Linux madvise residency 语义。
    pub(crate) fn advise_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        advice: MemoryAdvice,
    ) -> Result<(), MemoryError> {
        if length == 0 {
            return Ok(());
        }
        let range = Self::checked_page_range(address, length)?;
        let mut keys = Vec::new();
        let mut covered = range.start;
        for (key, area) in &self.areas {
            if range.start >= area.vpn_range.end || area.vpn_range.start >= range.end {
                continue;
            }
            if matches!(area.kind, VmaKind::System) || area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            if advice == MemoryAdvice::Free
                && (!matches!(area.kind, VmaKind::Anonymous | VmaKind::Stack { .. })
                    || area.shared_anonymous.is_some())
            {
                return Err(MemoryError::InvalidRange);
            }
            if advice == MemoryAdvice::DontNeed && area.device.is_some() {
                // Device extent 不可像 anonymous/page-cache residency 一样丢弃；若撤销 PTE
                // 却保留 VMA，后续 fault 没有 page-in owner，会把有效 GEM mapping 变成 SIGSEGV。
                return Err(MemoryError::InvalidRange);
            }
            covered = covered.max(area.vpn_range.end);
            keys.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
            keys.push(*key);
        }
        if covered < range.end {
            return Err(MemoryError::InvalidRange);
        }
        if matches!(
            advice,
            MemoryAdvice::Normal | MemoryAdvice::Random | MemoryAdvice::Sequential
        ) {
            return Ok(());
        }
        if advice == MemoryAdvice::WillNeed {
            for vpn in range.start.as_usize()..range.end.as_usize() {
                match self.handle_page_fault(vpn * config::PAGE_SIZE, PageFaultAccess::Read)? {
                    PageFaultOutcome::Handled => {}
                    PageFaultOutcome::SegmentationFault | PageFaultOutcome::BusError => {
                        return Err(MemoryError::InvalidRange);
                    }
                }
            }
            return Ok(());
        }
        if advice == MemoryAdvice::DontNeed {
            self.sync_shared_before_discard(&keys, range.clone())?;
            let memory = revoke_and_commit(self, |memory, commit| {
                memory.revoke_dontneed_translations(&keys, &range, commit);
            })
            .expect("platform TLB synchronization failed after MADV_DONTNEED");
            memory.release_dontneed_residents(&keys, &range);
            return Ok(());
        }

        debug_assert_eq!(advice, MemoryAdvice::Free);
        let mut commit = TranslationCommit::new();
        for key in keys {
            let area = self.areas.get_mut(&key).expect("validated VMA key");
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            for vpn in start.as_usize()..end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                if let Some(resident) = area.data_frames.get_mut(&vpn) {
                    resident.discardable = true;
                    if self.page_table.translate(vpn).is_some() {
                        let mut flags: PagePermissions = area.map_permission.into();
                        flags.remove(PagePermissions::WRITE);
                        self.page_table.set_flags(vpn, flags, &mut commit)?;
                    }
                }
            }
        }
        commit
            .synchronize()
            .expect("platform translation fence failed after MADV_FREE permission update");
        Ok(())
    }

    fn revoke_dontneed_translations(
        &mut self,
        keys: &[VirtualPageNumber],
        range: &Range<VirtualPageNumber>,
        commit: &mut TranslationCommit,
    ) {
        let page_table = &mut self.page_table;
        for key in keys {
            let area = self.areas.get_mut(key).expect("validated VMA key");
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            for vpn in start.as_usize()..end.as_usize() {
                let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn), commit);
            }
        }
    }

    fn release_dontneed_residents(
        &mut self,
        keys: &[VirtualPageNumber],
        range: &Range<VirtualPageNumber>,
    ) {
        for key in keys {
            let area = self.areas.get_mut(key).expect("validated VMA key");
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            for vpn in start.as_usize()..end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                area.data_frames.remove(&vpn);
                if let Some(shared) = &mut area.shared_file {
                    shared.resident.remove(&vpn);
                }
            }
        }
    }

    fn sync_shared_before_discard(
        &self,
        keys: &[VirtualPageNumber],
        range: Range<VirtualPageNumber>,
    ) -> Result<(), MemoryError> {
        for key in keys {
            let area = &self.areas[key];
            let Some(shared) = &area.shared_file else {
                continue;
            };
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            shared.sync_vma_range(area.vpn_range.start, start, end)?;
        }
        Ok(())
    }

    fn next_private_resident_from(
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

    /// @description 有界回收 MADV_FREE 页与可从 immutable backing 重建的 clean private 页。
    ///
    /// @param request 需要释放的物理页目标与 resident entry 扫描上限。
    /// @return 实际释放和扫描的页数；共享 COW frame 只撤销本 mm 映射，不伪报释放。
    pub(crate) fn reclaim_private_pages(&mut self, request: ReclaimRequest) -> ReclaimResult {
        if request.target_pages() == 0 || request.scan_pages() == 0 {
            return ReclaimResult::default();
        }
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
            self.revoke_unique_candidates += usize::from(Arc::strong_count(&resident.frame) == 1);
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
                Arc::strong_count(&resident.frame),
            );
            if !decision.release {
                // PTE 已在 revoke 阶段撤销；保留 resident owner 后，后续 fault 可直接重建
                // translation，同时保证 adapter result 不超过 caller target。
                continue;
            }
            let removed = area.data_frames.remove(&vpn);
            assert!(
                removed.is_some(),
                "private reclaim replay lost selected page"
            );
            reclaimed += usize::from(decision.reclaimed);
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
