use super::*;

mod advice;
mod anonymous_shared;
mod device;
mod fault;
mod protection;

/// @description 一次用户页访问 fault 的领域结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageFaultOutcome {
    /// 请求的访问权限已经由 live leaf PTE 满足，原指令可直接重试。
    Handled,
    /// 地址不属于允许该访问的用户 VMA。
    SegmentationFault,
    /// file mapping 地址属于 VMA，但已越过 backing object 的有效范围。
    BusError,
}

impl MemorySet {
    /// 使用 ordered neighbors 判断 `[start,end)` 是否与任意 live VMA 相交。
    ///
    /// @param start inclusive 起始 VPN。
    /// @param end exclusive 结束 VPN。
    /// @return 区间非空且 floor/ceiling 均不相交时为 true。
    pub(super) fn range_is_free(&self, start: VirtualPageNumber, end: VirtualPageNumber) -> bool {
        if start >= end
            || self
                .areas
                .floor(&start)
                .is_some_and(|(_, area)| start < area.vpn_range.end)
        {
            return false;
        }
        self.areas
            .ceiling(&start)
            .is_none_or(|(key, _)| *key >= end)
    }

    fn find_free_user_range(
        &self,
        first: VirtualPageNumber,
        page_count: usize,
    ) -> Option<Range<VirtualPageNumber>> {
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
        let mut start = first.as_usize().max(1);
        for area in self.areas.values() {
            let area_start = area.vpn_range.start.as_usize();
            let area_end = area.vpn_range.end.as_usize();
            if area_end <= start {
                continue;
            }
            if let Some(end) = start.checked_add(page_count)
                && end <= area_start.min(user_end)
            {
                return Some(start.into()..end.into());
            }
            start = start.max(area_end);
            if start >= user_end {
                return None;
            }
        }
        let end = start.checked_add(page_count)?;
        (end <= user_end).then(|| start.into()..end.into())
    }

    /// @description 建立按需分配的 anonymous private 用户映射。
    ///
    /// @param address 零表示由内核选址；非零是 page-aligned hint 或 fixed-noreplace 地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 用户页权限；必须含 U，允许 PROT_NONE 与 Linux W+X 映射。
    /// @param fixed_noreplace 为真时地址冲突返回 `AddressInUse`，不替换既有 VMA。
    /// @return 成功返回 page-aligned 起始地址；任何失败都不改变页表或 VMA 表。
    pub(crate) fn map_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            address_space_limit,
            permission.contains(MapPermission::W).then_some(data_limit),
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end_vpn = config::USER_ADDRESS_END / config::PAGE_SIZE;
        let hint_is_valid = address != 0
            && hinted_start.as_usize() < user_end_vpn
            && hinted_end.as_usize() <= user_end_vpn;
        let range = if hint_is_valid && self.range_is_free(hinted_start, hinted_end) {
            hinted_start..hinted_end
        } else if fixed_noreplace {
            return Err(if hint_is_valid {
                MemoryError::AddressInUse
            } else {
                MemoryError::InvalidRange
            });
        } else {
            self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), page_count)
                .ok_or(MemoryError::OutOfMemory)?
        };
        let start_address = usize::from(VirtualAddress::from(range.start));
        let end_address = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::anonymous(start_address.into(), end_address.into(), permission),
            None,
        )?;
        self.merge_anonymous_neighbors(range.start);
        Ok(start_address)
    }

    /// @description 建立按需 fault 的 file-backed private 映射。
    pub(crate) fn map_private_file(
        &mut self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource { mapping, pages } = file;
        if !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = usize::try_from(pages.count()).map_err(|_| MemoryError::InvalidRange)?;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            limits.address_space,
            permission.contains(MapPermission::W).then_some(limits.data),
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
        let hint_is_valid =
            address != 0 && hinted_start.as_usize() < user_end && hinted_end.as_usize() <= user_end;
        let range = if hint_is_valid && self.range_is_free(hinted_start, hinted_end) {
            hinted_start..hinted_end
        } else if fixed_noreplace {
            return Err(if hint_is_valid {
                MemoryError::AddressInUse
            } else {
                MemoryError::InvalidRange
            });
        } else {
            self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), page_count)
                .ok_or(MemoryError::OutOfMemory)?
        };
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        let backing = PrivateFileArea::cached_file(mapping, start, pages);
        self.push(
            MapArea::file(start.into(), end.into(), permission, backing),
            None,
        )?;
        Ok(start)
    }

    /// @description 建立 lazy file-backed shared mapping；page cache 是所有 resident page 的唯一 owner。
    pub(crate) fn map_shared_file(
        &mut self,
        address: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource { mapping, pages } = file;
        if !permission.contains(MapPermission::U)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = usize::try_from(pages.count()).map_err(|_| MemoryError::InvalidRange)?;
        self.ensure_resource_capacity(
            page_count as u64 * config::PAGE_SIZE as u64,
            address_space_limit,
            None,
        )?;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = config::USER_ADDRESS_END / config::PAGE_SIZE;
        let hint_is_valid =
            address != 0 && hinted_start.as_usize() < user_end && hinted_end.as_usize() <= user_end;
        let range = if hint_is_valid && self.range_is_free(hinted_start, hinted_end) {
            hinted_start..hinted_end
        } else if fixed_noreplace {
            return Err(if hint_is_valid {
                MemoryError::AddressInUse
            } else {
                MemoryError::InvalidRange
            });
        } else {
            self.find_free_user_range(VirtualAddress::from(Self::MMAP_BASE).floor(), page_count)
                .ok_or(MemoryError::OutOfMemory)?
        };
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::shared_file(start.into(), end.into(), permission, mapping, pages),
            None,
        )?;
        Ok(start)
    }

    pub(crate) fn sync_shared_mapping(
        &self,
        address: usize,
        length: usize,
        writeback: bool,
    ) -> Result<(), MemoryError> {
        let range = Self::checked_page_range(address, length)?;
        let mut covered = range.start;
        for area in self
            .areas
            .values()
            .filter(|area| range.start < area.vpn_range.end && area.vpn_range.start < range.end)
        {
            if area.vpn_range.start > covered {
                return Err(MemoryError::InvalidRange);
            }
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            if writeback && let Some(shared) = &area.shared_file {
                shared.sync_vma_range(area.vpn_range.start, start, end)?;
            }
            covered = covered.max(area.vpn_range.end);
        }
        (covered >= range.end)
            .then_some(())
            .ok_or(MemoryError::InvalidRange)
    }

    pub(crate) fn invalidate_shared_file(&mut self, id: SharedFileId, size: u64) {
        let memory = revoke_and_commit(self, |memory, commit| {
            memory.revoke_invalidated_file_translations(id, size, commit);
        })
        .expect("platform TLB synchronization failed after truncate invalidation");
        memory.release_invalidated_file_residents(id, size);
    }

    fn invalidated_file_starts(
        area: &MapArea,
        id: SharedFileId,
        size: u64,
    ) -> (Option<VirtualPageNumber>, Option<VirtualPageNumber>) {
        let start = area.vpn_range.start;
        let shared = area
            .shared_file
            .as_ref()
            .filter(|shared| shared.mapping.id() == id)
            .and_then(|shared| shared.pages.prefix_before(size))
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| start.as_usize().checked_add(offset))
            .map(VirtualPageNumber::from_vpn);
        let private = area
            .private_file
            .as_ref()
            .and_then(|private| private.first_stale_page(start, id, size));
        (shared, private)
    }

    fn revoke_invalidated_file_translations(
        &mut self,
        id: SharedFileId,
        size: u64,
        commit: &mut TranslationCommit,
    ) {
        let page_table = &mut self.page_table;
        self.areas.for_each_mut(|_, area| {
            let (shared_start, private_start) = Self::invalidated_file_starts(area, id, size);
            if let Some(first_stale) = shared_start
                && let Some(shared) = &area.shared_file
            {
                for (&vpn, _) in shared.resident.iter_from(&first_stale) {
                    let _ = page_table.unmap(vpn, commit);
                }
            }
            if let Some(first_stale) = private_start {
                for (&vpn, _) in area.data_frames.iter_from(&first_stale) {
                    let _ = page_table.unmap(vpn, commit);
                }
            }
        });
    }

    fn release_invalidated_file_residents(&mut self, id: SharedFileId, size: u64) {
        self.areas.for_each_mut(|_, area| {
            let (shared_start, private_start) = Self::invalidated_file_starts(area, id, size);
            if let Some(first_stale) = shared_start
                && let Some(shared) = &mut area.shared_file
            {
                drop(shared.resident.split_off(&first_stale));
            }
            if let Some(first_stale) = private_start {
                // Linux truncate 的 even_cows 语义撤销 EOF 外 private residency，包括
                // operation snapshot 后、truncate callback 取得 mm lock 前发布的页。
                drop(area.data_frames.split_off(&first_stale));
            }
        });
    }

    fn overlapping_mmap_keys(
        &self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> Result<Vec<VirtualPageNumber>, MemoryError> {
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if start < area.vpn_range.end && area.vpn_range.start < end {
                if !matches!(
                    area.kind,
                    VmaKind::Anonymous | VmaKind::File | VmaKind::Device
                ) {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
                keys.push(*key);
            }
        }
        Ok(keys)
    }

    fn merge_anonymous_neighbors(&mut self, key: VirtualPageNumber) {
        let Some(mut current_key) = self
            .areas
            .floor(&key)
            .filter(|(_, area)| key < area.vpn_range.end)
            .map(|(key, _)| *key)
            .or_else(|| self.areas.ceiling(&key).map(|(key, _)| *key))
        else {
            return;
        };

        if let Some(left_key) = self
            .areas
            .predecessor(&current_key)
            .filter(|(_, left)| left.anonymous_mergeable(&self.areas[&current_key]))
            .map(|(key, _)| *key)
        {
            self.merge_anonymous_pair(left_key, current_key);
            current_key = left_key;
        }
        while let Some(right_key) = self
            .areas
            .successor(&current_key)
            .filter(|(_, right)| self.areas[&current_key].anonymous_mergeable(right))
            .map(|(key, _)| *key)
        {
            self.merge_anonymous_pair(current_key, right_key);
        }
    }

    fn merge_anonymous_pair(&mut self, left_key: VirtualPageNumber, right_key: VirtualPageNumber) {
        let mut left = self
            .take_area_entry(&left_key)
            .expect("merge left VMA must remain present");
        let right = self
            .remove_area(&right_key)
            .expect("merge right VMA must remain present");
        left.value_mut().merge_anonymous(right);
        self.commit_area(left);
    }

    /// @description 解除 anonymous 或 file-backed private 页；未映射洞按 Linux 语义忽略。
    ///
    /// @param address page-aligned 起始地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @return 成功返回空值；若触及非 anonymous VMA 则保持全部映射不变并拒绝。
    pub(crate) fn unmap_user_mapping(
        &mut self,
        address: usize,
        length: usize,
    ) -> Result<(), MemoryError> {
        let range = Self::checked_page_range(address, length)?;
        let keys = self.overlapping_mmap_keys(range.start, range.end)?;
        // 1. 只计算真正保留的 left/right segment，避免完整删除也临时占用两枚节点。
        let slot_count = keys.iter().try_fold(0usize, |count, key| {
            let area = &self.areas[key];
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            count
                .checked_add(usize::from(area.vpn_range.start < cut_start))
                .and_then(|count| count.checked_add(usize::from(cut_end < area.vpn_range.end)))
                .ok_or(MemoryError::OutOfMemory)
        })?;
        // 2. 在 sync、PTE 撤销与 VMA removal 前完成全部可失败分配。
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
        // 被切除的 area 必须在 remote fence 完成前保留全部 frame/device/writer owner。
        // 缺少该 preflight 会在 PTE 已撤销后因 retire Vec OOM 无法安全保活 owner。
        let mut retired_areas = Vec::new();
        retired_areas
            .try_reserve_exact(keys.len())
            .map_err(|_| MemoryError::OutOfMemory)?;
        // 3. 从这里开始只有 writeback/PTE 领域错误，VMA segment publication 不再分配。
        for key in &keys {
            let area = &self.areas[key];
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            if let Some(shared) = &area.shared_file {
                shared.sync_vma_range(area.vpn_range.start, start, end)?;
            }
        }
        let mut segment_slots = segment_slots.into_iter();
        for key in keys {
            let area = self.remove_area(&key).unwrap();
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            let (left, middle, right) = area.partition_protectable(cut_start, cut_end);
            if let Some(left) = left {
                let slot = segment_slots.next().expect("preflighted left VMA slot");
                self.commit_area(slot.fill(left.vpn_range.start, left));
            }
            if let Some(right) = right {
                let slot = segment_slots.next().expect("preflighted right VMA slot");
                self.commit_area(slot.fill(right.vpn_range.start, right));
            }
            retired_areas.push(middle);
        }
        assert!(
            self.range_is_free(range.start, range.end),
            "preflighted munmap range remained published"
        );
        let retired_areas = revoke_and_commit(retired_areas, |areas, commit| {
            for area in areas {
                area.unmap(&mut self.page_table, commit);
            }
        })
        .expect("platform TLB synchronization failed after munmap page-table update");
        drop(retired_areas);
        Ok(())
    }

    fn checked_page_range(
        address: usize,
        length: usize,
    ) -> Result<Range<VirtualPageNumber>, MemoryError> {
        if address == 0 || length == 0 || !VirtualAddress::from(address).is_aligned() {
            return Err(MemoryError::InvalidRange);
        }
        let end = address
            .checked_add(length)
            .and_then(|value| value.checked_add(config::PAGE_SIZE - 1))
            .map(|value| value / config::PAGE_SIZE * config::PAGE_SIZE)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = config::USER_ADDRESS_END;
        if end > user_end {
            return Err(MemoryError::InvalidRange);
        }
        Ok(VirtualAddress::from(address).floor()..VirtualAddress::from(end).floor())
    }
}
