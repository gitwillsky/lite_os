use super::*;

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
            covered = covered.max(area.vpn_range.end);
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
                    area.discardable.remove(&vpn);
                    area.dirty_private.remove(&vpn);
                    if let Some(shared) = &mut area.shared_file {
                        shared.resident.remove(&vpn);
                    }
                } else if area.data_frames.contains_key(&vpn) {
                    area.discardable.insert(vpn);
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

    /// @description 回收 MADV_FREE 页与可从 immutable backing 重建的 clean private 页。
    pub(crate) fn reclaim_private_pages(&mut self, limit: usize) -> usize {
        let mut reclaimed = 0;
        for area in self.areas.values_mut() {
            while reclaimed < limit {
                // OOM reclaim 不能构造临时 Vec，否则 global allocator 会递归进入 frame growth。
                let Some(vpn) = area.data_frames.keys().copied().find(|vpn| {
                    area.discardable.contains(vpn)
                        || area.private_file.is_some() && !area.dirty_private.contains(vpn)
                }) else {
                    break;
                };
                let frees_frame = area
                    .data_frames
                    .get(&vpn)
                    .is_some_and(|frame| Arc::strong_count(frame) == 1);
                let _ = self.page_table.unmap(vpn);
                area.data_frames.remove(&vpn);
                area.discardable.remove(&vpn);
                area.dirty_private.remove(&vpn);
                reclaimed += usize::from(frees_frame);
            }
            if reclaimed >= limit {
                break;
            }
        }
        if reclaimed != 0 {
            Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after private page reclaim");
        }
        reclaimed
    }
}
