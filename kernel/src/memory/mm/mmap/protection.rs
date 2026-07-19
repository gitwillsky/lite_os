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
                if !matches!(
                    area.kind,
                    VmaKind::Anonymous | VmaKind::Elf | VmaKind::File | VmaKind::Device
                ) || area.device.is_some() && permission.contains(MapPermission::X)
                {
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
        let mut commit = TranslationCommit::new();
        for key in &keys {
            let area = self.remove_area(key).unwrap();
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(start, end);
            if middle.device.is_some() {
                middle.protect_device_area(
                    &mut self.page_table,
                    start..end,
                    permission,
                    &mut commit,
                )?;
            } else if middle.shared_file.is_some() {
                self.protect_shared_file(&mut middle, start, end, permission, &mut commit)?;
            } else {
                self.protect_private(&mut middle, start, end, permission, &mut commit)?;
            }
            middle.map_permission = permission;
            for segment in [left, Some(middle), right].into_iter().flatten() {
                let slot = segment_slots.next().expect("preflighted split VMA slot");
                self.commit_area(slot.fill(segment.vpn_range.start, segment));
            }
        }
        for key in keys {
            self.merge_anonymous_neighbors(key);
        }
        commit
            .synchronize()
            .expect("platform translation fence failed after mprotect update");
        self.release_restricted_shared_writers(range.start, range.end);
        Ok(())
    }

    /// 在 permission restriction 的 remote fence 完成后释放 shared-file writer claim。
    ///
    /// PTE 已撤销但远端 hart 仍可能使用旧 writable translation；提前释放 claim 会让
    /// writeback 与该 stale writer 并发。权限增加在 PTE 发布前 acquire，权限收紧则由此
    /// post-fence pass 释放，因而 writer 生命周期始终覆盖所有可写 translation。
    fn release_restricted_shared_writers(
        &mut self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) {
        self.areas.for_each_mut(|_, area| {
            if end <= area.vpn_range.start
                || area.vpn_range.end <= start
                || area.map_permission.contains(MapPermission::W)
            {
                return;
            }
            let Some(shared) = &mut area.shared_file else {
                return;
            };
            shared.resident.for_each_mut(|_, resident| {
                if resident.writer {
                    resident.page.release_writer();
                    resident.writer = false;
                }
            });
        });
    }

    fn protect_shared_file(
        &mut self,
        area: &mut MapArea,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
        permission: MapPermission,
        commit: &mut TranslationCommit,
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
            if writer && !resident.writer {
                resident.page.acquire_writer();
                resident.writer = writer;
            }
            let flags = permission.into();
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags, commit)?,
                (true, false) => self.page_table.unmap(vpn, commit)?,
                (false, true) => {
                    self.page_table
                        .map(vpn, resident.page.frame().ppn(), flags, commit)?
                }
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
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        let old_leaf = MapArea::has_leaf_permission(area.map_permission);
        let new_leaf = MapArea::has_leaf_permission(permission);
        for vpn in start.as_usize()..end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            let Some(frame) = area.data_frames.get(&vpn) else {
                continue;
            };
            let mut flags: PagePermissions = permission.into();
            if permission.contains(MapPermission::W)
                && area.shared_anonymous.is_none()
                && (Arc::strong_count(&frame.frame) > 1
                    || area.private_file.is_some() && !frame.dirty)
            {
                flags.remove(PagePermissions::WRITE);
            }
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags, commit)?,
                (true, false) => self.page_table.unmap(vpn, commit)?,
                (false, true) => self.page_table.map(vpn, frame.ppn, flags, commit)?,
                (false, false) => {}
            }
        }
        Ok(())
    }
}
