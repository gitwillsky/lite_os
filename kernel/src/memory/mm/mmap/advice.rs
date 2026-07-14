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
        }
        for key in keys {
            let area = self.areas.get_mut(&key).expect("validated VMA key");
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            for vpn in start.as_usize()..end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                if advice == MemoryAdvice::DontNeed {
                    let _ = self.page_table.unmap(vpn);
                    area.data_frames.remove(&vpn);
                    if let Some(shared) = &mut area.shared_file {
                        shared.resident.remove(&vpn);
                    }
                } else if let Some(resident) = area.data_frames.get_mut(&vpn) {
                    resident.discardable = true;
                    if self.page_table.translate(vpn).is_some() {
                        let mut flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
                        flags.remove(PTEFlags::W);
                        self.page_table.set_flags(vpn, flags)?;
                    }
                }
            }
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after madvise residency update");
        Ok(())
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
            let offset = shared.file_offset
                + (start.as_usize() - area.vpn_range.start.as_usize()) as u64
                    * config::PAGE_SIZE as u64;
            let bytes = (end.as_usize() - start.as_usize()) as u64 * config::PAGE_SIZE as u64;
            shared
                .mapping
                .sync_range(offset, bytes)
                .map_err(|error| match error {
                    SharedFileError::OutOfMemory => MemoryError::OutOfMemory,
                    SharedFileError::Io => MemoryError::Io,
                    SharedFileError::BeyondEof => MemoryError::InvalidRange,
                })?;
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
            && let Some((&vpn, _)) = area.data_frames.iter_from(&cursor).next()
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
        let initial_cursor = self.private_reclaim_cursor;
        let mut wrapped = false;
        let mut reclaimed = 0;
        let mut scanned = 0;
        let mut unmapped = false;
        while reclaimed < request.target_pages() && scanned < request.scan_pages() {
            // 1. 持久 cursor 到达 VMA 末尾时只回绕一次；wrap 后再到初始
            // cursor 即结束，不为计算 resident 数先全表扫描或重复访问 entry。
            let next = self.next_private_resident_from(self.private_reclaim_cursor);
            let Some((area_key, vpn)) = next else {
                if wrapped {
                    break;
                }
                self.private_reclaim_cursor = VirtualPageNumber::from_vpn(0);
                wrapped = true;
                continue;
            };
            if wrapped && vpn >= initial_cursor {
                break;
            }
            self.private_reclaim_cursor = vpn
                .as_usize()
                .checked_add(1)
                .map(VirtualPageNumber::from_vpn)
                .unwrap_or_else(|| VirtualPageNumber::from_vpn(0));
            scanned += 1;

            // 2. 是否可丢弃只由 VMA owner 的单一 resident record 决定。
            let area = self
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
            let frees_frame = Arc::strong_count(&resident.frame) == 1;
            match self.page_table.unmap(vpn) {
                Ok(()) => unmapped = true,
                Err(PageTableError::NotMapped) => {}
                Err(error) => panic!("private reclaim failed to unmap {vpn:?}: {error:?}"),
            }
            let removed = area.data_frames.remove(&vpn);
            debug_assert!(removed.is_some());
            reclaimed += usize::from(frees_frame);
        }
        // 3. COW frame 仍被其他 mm 引用时 reclaimed==0，但本 mm 的 leaf PTE 已撤销；
        // 若只按物理页计数 flush，当前 hart 可继续命中 stale writable translation。
        if unmapped {
            Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after private page reclaim");
        }
        ReclaimResult::new(reclaimed, scanned)
    }
}
