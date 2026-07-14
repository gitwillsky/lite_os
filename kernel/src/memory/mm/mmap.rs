use super::*;

mod advice;
mod anonymous_shared;
mod protection;

/// @description 用户 page fault 请求的访问类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageFaultAccess {
    /// 读取用户页。
    Read,
    /// 写入用户页。
    Write,
    /// 执行用户页指令。
    Execute,
}

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
        self.merge_adjacent_anonymous();
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after mmap page-table update");
        Ok(start_address)
    }

    /// @description 建立按需 fault 的 file-backed private 映射。
    pub(crate) fn map_private_file(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        file: FileMappingSource,
        limits: MappingResourceLimits,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource {
            mapping,
            offset: file_offset,
        } = file;
        if length == 0
            || !file_offset.is_multiple_of(config::PAGE_SIZE as u64)
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
            limits.address_space,
            permission.contains(MapPermission::W).then_some(limits.data),
        )?;
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
        let source_offset = usize::try_from(file_offset).map_err(|_| MemoryError::InvalidRange)?;
        let backing = PrivateFileArea::cached_file(mapping, start, source_offset);
        self.push(
            MapArea::file(start.into(), end.into(), permission, backing),
            None,
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
        file: FileMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        let FileMappingSource {
            mapping,
            offset: file_offset,
        } = file;
        if length == 0
            || !file_offset.is_multiple_of(config::PAGE_SIZE as u64)
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
            None,
        )?;
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
        self.handle_page_fault_with_limits(address, access, UserFaultLimits::initial_exec())
    }

    pub(crate) fn handle_page_fault_with_limits(
        &mut self,
        address: usize,
        access: PageFaultAccess,
        limits: UserFaultLimits,
    ) -> Result<PageFaultOutcome, MemoryError> {
        let vpn = VirtualAddress::from(address).floor();
        self.grow_stack_for_fault(address, limits.stack, limits.address_space)?;
        let needs_private_frame = self
            .areas
            .range(..=vpn)
            .next_back()
            .map(|(_, area)| {
                vpn < area.vpn_range.end
                    && area.lazy_private
                    && area.shared_anonymous.is_none()
                    && area.shared_file.is_none()
                    && !area.data_frames.contains_key(&vpn)
            })
            .unwrap_or(false);
        let mut prepared_private_frame = if needs_private_frame {
            Some(self.allocate_private_frame()?)
        } else {
            None
        };
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
        if area
            .private_file
            .as_ref()
            .is_some_and(|backing| !backing.faultable(vpn))
        {
            return Ok(PageFaultOutcome::BusError);
        }
        if let Some(shared) = &area.shared_anonymous {
            if !area.data_frames.contains_key(&vpn) {
                let index = shared.page_offset
                    + vpn
                        .as_usize()
                        .saturating_sub(area.vpn_range.start.as_usize());
                let frame = shared.backing.page(index)?;
                self.page_table.map(
                    vpn,
                    frame.ppn,
                    PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
                )?;
                area.data_frames.insert(vpn, frame);
                Self::flush_tlb_all_cpus()
                    .expect("SBI RFENCE failed after shared anonymous page fault");
                return Ok(PageFaultOutcome::Handled);
            }
            if self.page_table.translate(vpn).is_none() {
                let frame = area.data_frames.get(&vpn).expect("resident shared page");
                self.page_table.map(
                    vpn,
                    frame.ppn,
                    PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
                )?;
                Self::flush_tlb_all_cpus()
                    .expect("SBI RFENCE failed after shared anonymous permission fault");
            }
            return Ok(PageFaultOutcome::Handled);
        }
        if area.shared_file.is_none() {
            if access == PageFaultAccess::Write && area.discardable.remove(&vpn) {
                return match self.handle_cow_fault(address)? {
                    true => Ok(PageFaultOutcome::Handled),
                    false => Ok(PageFaultOutcome::SegmentationFault),
                };
            }
            if area.lazy_private && !area.data_frames.contains_key(&vpn) {
                let mut frame = prepared_private_frame
                    .take()
                    .ok_or(MemoryError::OutOfMemory)?;
                if let Some(backing) = &area.private_file {
                    backing.fill(vpn, &mut frame)?;
                }
                let ppn = frame.ppn;
                let mut flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
                if area.private_file.is_some() && area.map_permission.contains(MapPermission::W) {
                    // 首次 read 保持只读，后续 store fault 是标记 MAP_PRIVATE dirty 的唯一入口。
                    flags.remove(PTEFlags::W);
                    if access == PageFaultAccess::Write {
                        area.dirty_private.insert(vpn);
                        flags |= PTEFlags::W;
                    }
                }
                self.page_table.map(vpn, ppn, flags)?;
                area.data_frames.insert(vpn, Arc::new(frame));
                Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after private page fault");
                return Ok(PageFaultOutcome::Handled);
            }
            if access == PageFaultAccess::Write && area.private_file.is_some() {
                area.dirty_private.insert(vpn);
            }
            return match access {
                PageFaultAccess::Write if self.handle_cow_fault(address)? => {
                    Ok(PageFaultOutcome::Handled)
                }
                _ if self.page_table.translate(vpn).is_some() => Ok(PageFaultOutcome::Handled),
                _ => Ok(PageFaultOutcome::SegmentationFault),
            };
        }
        let shared = area.shared_file.as_mut().unwrap();
        if let Some(resident) = shared.resident.get(&vpn) {
            if self.page_table.translate(vpn).is_none() {
                self.page_table.map(
                    vpn,
                    resident.page.frame().ppn(),
                    PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
                )?;
                Self::flush_tlb_all_cpus()
                    .expect("SBI RFENCE failed after shared file permission fault");
            }
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

    fn allocate_private_frame(&mut self) -> Result<FrameTracker, MemoryError> {
        if let Some(frame) = alloc() {
            return Ok(frame);
        }
        // 1. 当前 mm 在已持有 AddressSpace lock 下直接回收；registry 会通过 try_lock 跳过它。
        self.reclaim_private_pages(64);
        // 2. alloc 的统一慢路径会在需要时再请求其他 resident owner，最后只重试一次。
        alloc().ok_or(MemoryError::OutOfMemory)
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
                left.anonymous_mergeable(right)
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
}
