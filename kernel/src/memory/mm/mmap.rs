use super::*;

impl MemorySet {
    fn range_is_free(&self, start: VirtualPageNumber, end: VirtualPageNumber) -> bool {
        start < end
            && !self
                .areas
                .values()
                .any(|area| start < area.vpn_range.end && area.vpn_range.start < end)
    }

    fn find_free_user_range(
        &self,
        first: VirtualPageNumber,
        page_count: usize,
    ) -> Option<Range<VirtualPageNumber>> {
        let user_end = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
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

    /// @description 建立 eager anonymous private 用户映射，VMA 表是区间与页帧的唯一 owner。
    ///
    /// @param address 零表示由内核选址；非零是 page-aligned hint 或 fixed-noreplace 地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 用户页权限；必须含 U，允许 PROT_NONE，禁止 W+X。
    /// @param fixed_noreplace 为真时地址冲突返回 `AddressInUse`，不替换既有 VMA。
    /// @return 成功返回 page-aligned 起始地址；任何失败都不改变页表或 VMA 表。
    pub(crate) fn map_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end_vpn = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
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
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mmap page-table update");
        Ok(start_address)
    }

    /// @description 建立 eager file-backed private 映射；VMA 独占映射后的私有页帧。
    pub(crate) fn map_private_file(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        data: &[u8],
    ) -> Result<usize, MemoryError> {
        if length == 0
            || data.len() > length
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
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
            MapArea::file(start.into(), end.into(), permission),
            Some(data),
        )?;
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after file mmap page-table update");
        Ok(start)
    }

    /// @description 建立 lazy file-backed shared mapping；page cache 是所有 resident page 的唯一 owner。
    pub(crate) fn map_shared_file(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        mapping: Arc<dyn SharedFileMapping>,
        file_offset: u64,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !file_offset.is_multiple_of(config::PAGE_SIZE as u64)
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        let hinted_start = VirtualAddress::from(address).floor();
        let hinted_end = hinted_start
            .as_usize()
            .checked_add(page_count)
            .map(VirtualPageNumber::from_vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let user_end = (1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1)) / config::PAGE_SIZE;
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
            MapArea::shared_file(start.into(), end.into(), permission, mapping, file_offset),
            None,
        )?;
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after shared mmap update");
        Ok(start)
    }

    pub(crate) fn handle_page_fault(
        &mut self,
        address: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultOutcome, MemoryError> {
        let vpn = VirtualAddress::from(address).floor();
        let Some((_, area)) = self.areas.range_mut(..=vpn).next_back() else {
            return Ok(PageFaultOutcome::SegmentationFault);
        };
        if vpn >= area.vpn_range.end || !area.map_permission.contains(MapPermission::U) {
            return Ok(PageFaultOutcome::SegmentationFault);
        }
        let permitted = match access {
            PageFaultAccess::Read => area.map_permission.contains(MapPermission::R),
            PageFaultAccess::Write => area.map_permission.contains(MapPermission::W),
            PageFaultAccess::Execute => area.map_permission.contains(MapPermission::X),
        };
        if !permitted {
            return Ok(PageFaultOutcome::SegmentationFault);
        }
        if area.shared_file.is_none() {
            return match access {
                PageFaultAccess::Write if self.handle_cow_fault(address)? => {
                    Ok(PageFaultOutcome::Handled)
                }
                _ => Ok(PageFaultOutcome::SegmentationFault),
            };
        }
        let shared = area.shared_file.as_mut().unwrap();
        if shared.resident.contains_key(&vpn) {
            return Ok(PageFaultOutcome::Handled);
        }
        let index = (shared.file_offset / config::PAGE_SIZE as u64)
            + (vpn.as_usize() - area.vpn_range.start.as_usize()) as u64;
        if index * config::PAGE_SIZE as u64 >= shared.mapping.size() {
            return Ok(PageFaultOutcome::BusError);
        }
        let page = shared.mapping.page(index).map_err(|error| match error {
            SharedFileError::OutOfMemory => MemoryError::OutOfMemory,
            SharedFileError::Io => MemoryError::Io,
            SharedFileError::BeyondEof => MemoryError::InvalidRange,
        })?;
        let resident = SharedResident::new(page, area.map_permission.contains(MapPermission::W));
        let flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
        self.page_table
            .map(vpn, resident.page.frame().ppn(), flags)?;
        shared.resident.insert(vpn, resident);
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after shared page fault");
        Ok(PageFaultOutcome::Handled)
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
            covered = covered.max(area.vpn_range.end);
        }
        (covered >= range.end)
            .then_some(())
            .ok_or(MemoryError::InvalidRange)
    }

    pub(crate) fn invalidate_shared_file(&mut self, id: SharedFileId, size: u64) {
        for area in self.areas.values_mut() {
            let Some(shared) = &mut area.shared_file else {
                continue;
            };
            if shared.mapping.id() != id {
                continue;
            }
            let start = area.vpn_range.start;
            let file_offset = shared.file_offset;
            let stale: Vec<_> = shared
                .resident
                .keys()
                .copied()
                .filter(|vpn| {
                    file_offset
                        + (vpn.as_usize() - start.as_usize()) as u64 * config::PAGE_SIZE as u64
                        >= size
                })
                .collect();
            for vpn in stale {
                let _ = self.page_table.unmap(vpn);
                self.page_table
                    .reserve(vpn)
                    .expect("invalidated shared slot must be reusable");
                shared.resident.remove(&vpn);
            }
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after truncate invalidation");
    }

    fn overlapping_mmap_keys(
        &self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> Result<Vec<VirtualPageNumber>, MemoryError> {
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if start < area.vpn_range.end && area.vpn_range.start < end {
                if !matches!(area.kind, VmaKind::Anonymous | VmaKind::File) {
                    return Err(MemoryError::PermissionDenied);
                }
                keys.push(*key);
            }
        }
        Ok(keys)
    }

    fn merge_adjacent_anonymous(&mut self) {
        loop {
            let keys: Vec<_> = self.areas.keys().copied().collect();
            let Some((left_key, right_key)) = keys.windows(2).find_map(|pair| {
                let left = &self.areas[&pair[0]];
                let right = &self.areas[&pair[1]];
                (left.kind == VmaKind::Anonymous
                    && right.kind == VmaKind::Anonymous
                    && left.vpn_range.end == right.vpn_range.start
                    && left.map_permission == right.map_permission)
                    .then_some((pair[0], pair[1]))
            }) else {
                break;
            };
            let left = self.areas.remove(&left_key).unwrap();
            let right = self.areas.remove(&right_key).unwrap();
            self.areas.insert(left_key, left.merge_anonymous(right));
        }
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
        for key in &keys {
            let area = &self.areas[key];
            let start = range.start.max(area.vpn_range.start);
            let end = range.end.min(area.vpn_range.end);
            if let Some(shared) = &area.shared_file {
                let offset = shared.file_offset
                    + (start.as_usize() - area.vpn_range.start.as_usize()) as u64
                        * config::PAGE_SIZE as u64;
                let length = (end.as_usize() - start.as_usize()) as u64 * config::PAGE_SIZE as u64;
                shared
                    .mapping
                    .sync_range(offset, length)
                    .map_err(|error| match error {
                        SharedFileError::OutOfMemory => MemoryError::OutOfMemory,
                        SharedFileError::Io => MemoryError::Io,
                        SharedFileError::BeyondEof => MemoryError::InvalidRange,
                    })?;
            }
        }
        for key in keys {
            let area = self.areas.remove(&key).unwrap();
            let cut_start = range.start.max(area.vpn_range.start);
            let cut_end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(cut_start, cut_end);
            middle.unmap(&mut self.page_table);
            if let Some(left) = left {
                self.areas.insert(left.vpn_range.start, left);
            }
            if let Some(right) = right {
                self.areas.insert(right.vpn_range.start, right);
            }
        }
        if !self.range_is_free(range.start, range.end) {
            return Err(MemoryError::PermissionDenied);
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after munmap page-table update");
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
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        if end > user_end {
            return Err(MemoryError::InvalidRange);
        }
        Ok(VirtualAddress::from(address).floor()..VirtualAddress::from(end).floor())
    }

    /// @description 修改完整 anonymous 或 ELF private 区间权限，并按边界拆分 VMA。
    ///
    /// @param address page-aligned 起始地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 新用户权限；允许 PROT_NONE，禁止 W+X。
    /// @return 成功返回空值；缺页或触及其他系统 VMA 时在修改前整体失败。
    pub(crate) fn protect_user_mapping(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        if !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::W | MapPermission::X)
        {
            return Err(MemoryError::InvalidRange);
        }
        let range = Self::checked_page_range(address, length)?;
        let mut keys = Vec::new();
        for (key, area) in &self.areas {
            if range.start < area.vpn_range.end && area.vpn_range.start < range.end {
                if !matches!(area.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File) {
                    return Err(MemoryError::PermissionDenied);
                }
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
        for key in keys {
            let area = self.areas.remove(&key).unwrap();
            let change_start = range.start.max(area.vpn_range.start);
            let change_end = range.end.min(area.vpn_range.end);
            let (left, mut middle, right) = area.partition_protectable(change_start, change_end);
            let old_has_leaf = MapArea::has_leaf_permission(middle.map_permission);
            let new_has_leaf = MapArea::has_leaf_permission(permission);
            if let Some(shared) = &mut middle.shared_file {
                for vpn in change_start.as_usize()..change_end.as_usize() {
                    let vpn = VirtualPageNumber::from_vpn(vpn);
                    let Some(resident) = shared.resident.get_mut(&vpn) else {
                        continue;
                    };
                    let new_writer = permission.contains(MapPermission::W);
                    if resident.writer != new_writer {
                        if new_writer {
                            resident.page.acquire_writer();
                        } else {
                            resident.page.release_writer();
                        }
                        resident.writer = new_writer;
                    }
                    match (old_has_leaf, new_has_leaf) {
                        (true, true) => self
                            .page_table
                            .set_flags(vpn, PTEFlags::from_bits(permission.bits()).unwrap())?,
                        (true, false) => {
                            self.page_table.unmap(vpn)?;
                        }
                        (false, true) => self.page_table.map(
                            vpn,
                            resident.page.frame().ppn(),
                            PTEFlags::from_bits(permission.bits()).unwrap(),
                        )?,
                        (false, false) => {}
                    }
                }
                middle.map_permission = permission;
                for segment in [left, Some(middle), right].into_iter().flatten() {
                    self.areas.insert(segment.vpn_range.start, segment);
                }
                continue;
            }
            for vpn in change_start.as_usize()..change_end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                let mut pte_flags = PTEFlags::from_bits(permission.bits()).unwrap();
                if permission.contains(MapPermission::W)
                    && middle
                        .data_frames
                        .get(&vpn)
                        .is_some_and(|frame| Arc::strong_count(frame) > 1)
                {
                    pte_flags.remove(PTEFlags::W);
                }
                match (old_has_leaf, new_has_leaf) {
                    (true, true) => self
                        .page_table
                        .set_flags(vpn, pte_flags)
                        .expect("accessible anonymous VMA must own a leaf PTE"),
                    (true, false) => self
                        .page_table
                        .unmap(vpn)
                        .expect("accessible anonymous VMA must own a leaf PTE"),
                    (false, true) => {
                        let ppn = middle
                            .data_frames
                            .get(&vpn)
                            .expect("anonymous VMA must own every reserved frame")
                            .ppn;
                        self.page_table
                            .map(vpn, ppn, pte_flags)
                            .expect("PROT_NONE VMA must own an empty reserved leaf slot");
                    }
                    (false, false) => {}
                }
            }
            middle.map_permission = permission;
            for segment in [left, Some(middle), right].into_iter().flatten() {
                self.areas.insert(segment.vpn_range.start, segment);
            }
        }
        self.merge_adjacent_anonymous();
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mprotect page-table update");
        Ok(())
    }
}
