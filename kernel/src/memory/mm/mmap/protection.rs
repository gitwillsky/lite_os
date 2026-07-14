use super::*;

impl MemorySet {
    /// @description 修改完整 anonymous/file/ELF 区间权限，并按 VMA 边界原子拆分。
    pub(crate) fn protect_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        if !permission.contains(MapPermission::U) {
            return Err(MemoryError::InvalidRange);
        }
        let range = Self::checked_page_range(address, length)?;
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if range.start < area.vpn_range.end && area.vpn_range.start < range.end {
                if !matches!(area.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File) {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
                keys.push(*key);
            }
        }
        let mut covered = range.start;
        for key in &keys {
            let area = &self.areas[key];
            if area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            covered = covered.max(area.vpn_range.end);
        }
        if covered < range.end {
            return Err(MemoryError::InvalidRange);
        }
        // 1. middle 必然保留，left/right 只为实际边界计算节点，避免固定 3x staging。
        let slot_count = keys.iter().try_fold(0usize, |count, key| {
            let area = &self.areas[key];
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            count
                .checked_add(1 + usize::from(area.vpn_range.start < start))
                .and_then(|count| count.checked_add(usize::from(end < area.vpn_range.end)))
                .ok_or(MemoryError::OutOfMemory)
        })?;
        // 2. 在第一个 PTE flag 改变前完成全部 node 与 staging Vec 分配。
        let mut segment_slots = Vec::new();
        segment_slots
            .try_reserve_exact(slot_count)
            .map_err(|_| MemoryError::OutOfMemory)?;
        for _ in 0..slot_count {
            segment_slots.push(
                FallibleMap::<VirtualPageNumber, MapArea>::try_reserve_node()
                    .map_err(|_| MemoryError::OutOfMemory)?,
            );
        }
        // 3. 后续每个 segment 只消费预留 token，AVL rotation 与 commit 均不分配。
        let mut segment_slots = segment_slots.into_iter();
        for key in keys {
            let area = self.areas.remove(&key).unwrap();
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(start, end);
            if middle.shared_file.is_some() {
                self.protect_shared_file(&mut middle, start, end, permission)?;
            } else {
                self.protect_private(&mut middle, start, end, permission)?;
            }
            middle.map_permission = permission;
            for segment in [left, Some(middle), right].into_iter().flatten() {
                let slot = segment_slots.next().expect("preflighted split VMA slot");
                self.areas
                    .commit_vacant(slot.fill(segment.vpn_range.start, segment));
            }
        }
        self.merge_adjacent_anonymous();
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mprotect page-table update");
        Ok(())
    }

    fn protect_shared_file(
        &mut self,
        area: &mut MapArea,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        let old_leaf = MapArea::has_leaf_permission(area.map_permission);
        let new_leaf = MapArea::has_leaf_permission(permission);
        let shared = area.shared_file.as_mut().unwrap();
        for vpn in start.as_usize()..end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            let Some(resident) = shared.resident.get_mut(&vpn) else {
                continue;
            };
            let writer = permission.contains(MapPermission::W);
            if resident.writer != writer {
                if writer {
                    resident.page.acquire_writer();
                } else {
                    resident.page.release_writer();
                }
                resident.writer = writer;
            }
            let flags = PTEFlags::from_bits(permission.bits()).unwrap();
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags)?,
                (true, false) => self.page_table.unmap(vpn)?,
                (false, true) => self
                    .page_table
                    .map(vpn, resident.page.frame().ppn(), flags)?,
                (false, false) => {}
            }
        }
        Ok(())
    }

    fn protect_private(
        &mut self,
        area: &mut MapArea,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        let old_leaf = MapArea::has_leaf_permission(area.map_permission);
        let new_leaf = MapArea::has_leaf_permission(permission);
        for vpn in start.as_usize()..end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            let Some(frame) = area.data_frames.get(&vpn) else {
                continue;
            };
            let mut flags = PTEFlags::from_bits(permission.bits()).unwrap();
            if permission.contains(MapPermission::W)
                && area.shared_anonymous.is_none()
                && (Arc::strong_count(&frame.frame) > 1
                    || area.private_file.is_some() && !frame.dirty)
            {
                flags.remove(PTEFlags::W);
            }
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags)?,
                (true, false) => self.page_table.unmap(vpn)?,
                (false, true) => self.page_table.map(vpn, frame.ppn, flags)?,
                (false, false) => {}
            }
        }
        Ok(())
    }
}
