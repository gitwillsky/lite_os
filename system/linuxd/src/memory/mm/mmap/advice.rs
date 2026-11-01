use super::*;

impl MemorySet {
    /// 对完整用户 VMA 区间应用 Linux madvise residency 语义。
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
        let first = self
            .areas
            .floor(&range.start)
            .map_or(range.start, |(key, _)| *key);
        for (key, area) in self
            .areas
            .iter_from(&first)
            .take_while(|(key, _)| **key < range.end)
        {
            if range.start >= area.vpn_range.end {
                continue;
            }
            if matches!(area.kind, VmaKind::System) || area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            if advice == MemoryAdvice::Free
                && (!matches!(area.kind, VmaKind::Anonymous | VmaKind::Stack)
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
                    PageFaultOutcome::SegmentationFault(_) | PageFaultOutcome::BusError => {
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
}
