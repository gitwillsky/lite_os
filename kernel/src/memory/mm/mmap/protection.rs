use super::*;

impl MemorySet {
    /// 修改完整 anonymous/file/ELF 区间权限，并按 VMA 边界原子拆分。
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
        let first = self
            .areas
            .floor(&range.start)
            .map_or(range.start, |(key, _)| *key);
        for (key, area) in self
            .areas
            .iter_from(&first)
            .take_while(|(key, _)| **key < range.end)
        {
            if range.start < area.vpn_range.end {
                if !matches!(
                    area.kind,
                    VmaKind::Anonymous
                        | VmaKind::Stack
                        | VmaKind::Elf
                        | VmaKind::File
                        | VmaKind::Device(_)
                ) || area.device.is_some() && permission.contains(MapPermission::X)
                {
                    return Err(MemoryError::PermissionDenied);
                }
                if permission.contains(MapPermission::W)
                    && (area.shared_anonymous.as_ref().is_some_and(|shared| {
                        shared
                            .sysv
                            .as_ref()
                            .is_some_and(|membership| !membership.may_write)
                    }) || area
                        .shared_file
                        .as_ref()
                        .is_some_and(|shared| !shared.mapping.may_write()))
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
        // private reclaim 可保留 resident 而撤销 PTE；PROT_NONE 也会释放空 table。
        // Shared physical pins 等待既有迁移并检查安装 epoch；MM guard 随后保持到 mutation
        // 和 fence，后来的迁移会 try-MM 失败。所有等待/IO/OOM 必须在移除首 VMA 前完成。
        let mut shared_frames = Vec::new();
        if MapArea::has_leaf_permission(permission) {
            for key in &keys {
                let area = &self.areas[key];
                for (&vpn, resident) in &area.data_frames {
                    if range.contains(&vpn)
                        && let Some(pin) = resident.pin_resident()?
                    {
                        shared_frames
                            .try_reserve(1)
                            .map_err(|_| MemoryError::OutOfMemory)?;
                        shared_frames.push((vpn, pin.ppn()));
                    }
                }
                if let Some(shared) = &area.shared_file {
                    for (&vpn, resident) in &shared.resident {
                        if range.contains(&vpn)
                            && let Some(pin) = resident.pin_installed()?
                        {
                            shared_frames
                                .try_reserve(1)
                                .map_err(|_| MemoryError::OutOfMemory)?;
                            shared_frames.push((vpn, pin.ppn()));
                        }
                    }
                }
            }
        }
        // 在 VMA/PTE mutation 前为所有将要发布的 resident/device leaf 预留路径。
        if MapArea::has_leaf_permission(permission) {
            for key in &keys {
                let area = &self.areas[key];
                let start = range.start.max(area.vpn_range.start);
                let end = range.end.min(area.vpn_range.end);
                for vpn in start.as_usize()..end.as_usize() {
                    let vpn = VirtualPageNumber::from_vpn(vpn);
                    let resident = area.device.is_some()
                        || area.data_frames.contains_key(&vpn)
                        || area
                            .shared_file
                            .as_ref()
                            .is_some_and(|shared| shared.resident.contains_key(&vpn));
                    if resident && self.page_table.translate(vpn).is_none() {
                        self.page_table.reserve(vpn)?;
                    }
                }
            }
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
                self.protect_shared_file(
                    &mut middle,
                    start,
                    end,
                    permission,
                    &shared_frames,
                    &mut commit,
                )?;
            } else {
                self.protect_private(
                    &mut middle,
                    start,
                    end,
                    permission,
                    &shared_frames,
                    &mut commit,
                )?;
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

    fn protect_shared_file(
        &mut self,
        area: &mut MapArea,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
        permission: MapPermission,
        frames: &[(VirtualPageNumber, crate::memory::PhysicalPageNumber)],
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        let shared = area.shared_file.as_mut().unwrap();
        let permission = shared.leaf_permission(permission);
        let new_leaf = MapArea::has_leaf_permission(permission);
        for vpn in start.as_usize()..end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            let Some(resident) = shared.resident.get_mut(&vpn) else {
                continue;
            };
            let prepared_frame = frames
                .binary_search_by_key(&vpn, |&(vpn, _)| vpn)
                .ok()
                .map(|index| frames[index].1);
            if new_leaf && prepared_frame.is_none() {
                continue;
            }
            let writer = permission.contains(MapPermission::W);
            if writer && !resident.writer {
                resident.page.acquire_writer();
                resident.writer = writer;
            }
            let flags = permission.into();
            let old_leaf = self.page_table.translate(vpn).is_some();
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags, commit)?,
                (true, false) => self.page_table.unmap(vpn, commit)?,
                (false, true) => self.page_table.map(
                    vpn,
                    prepared_frame.expect("prepared shared physical leaf"),
                    flags,
                    commit,
                )?,
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
        frames: &[(VirtualPageNumber, crate::memory::PhysicalPageNumber)],
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        let new_leaf = MapArea::has_leaf_permission(permission);
        for vpn in start.as_usize()..end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            let Some(frame) = area.data_frames.get(&vpn) else {
                continue;
            };
            let prepared_frame = frames
                .binary_search_by_key(&vpn, |&(vpn, _)| vpn)
                .ok()
                .map(|index| frames[index].1);
            if new_leaf && prepared_frame.is_none() {
                continue;
            }
            let flags = if area.shared_anonymous.is_some() {
                permission.into()
            } else {
                frame.permissions(permission, area.private_file.is_some())
            };
            let old_leaf = self.page_table.translate(vpn).is_some();
            match (old_leaf, new_leaf) {
                (true, true) => self.page_table.set_flags(vpn, flags, commit)?,
                (true, false) => self.page_table.unmap(vpn, commit)?,
                (false, true) => self.page_table.map(
                    vpn,
                    prepared_frame.expect("prepared private physical leaf"),
                    flags,
                    commit,
                )?,
                (false, false) => {}
            }
        }
        Ok(())
    }
}
