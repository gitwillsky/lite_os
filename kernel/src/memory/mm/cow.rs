use super::*;

impl MemorySet {
    pub(super) fn prepare_user_read(
        &mut self,
        address: usize,
        length: usize,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(address, length)?;
        let mut current = address;
        while current < end {
            if self.user_page(current, PTEFlags::R).is_err() {
                match self.handle_page_fault(current, PageFaultAccess::Read) {
                    Ok(PageFaultOutcome::Handled) => {}
                    Err(MemoryError::OutOfMemory) => return Err(UserAccessError::OutOfMemory),
                    _ => return Err(UserAccessError::Fault),
                }
            }
            current = (current | (config::PAGE_SIZE - 1))
                .saturating_add(1)
                .min(end);
        }
        self.validate_user_range(address, length, PTEFlags::R)
    }

    /// @description 为 fork 共享用户 frame 并把可写映射转换为 COW；supervisor frame 仍独立复制。
    pub(crate) fn try_clone_for_fork(&mut self) -> Result<Self, MemoryError> {
        let mut cloned = Self::try_new()?;
        cloned.map_trampoline()?;
        for (key, area) in &mut self.areas {
            let cloned_area = if area.map_permission.contains(MapPermission::U) {
                if let Some(shared) = &area.shared_file {
                    let mut resident = BTreeMap::new();
                    for (&vpn, source) in &shared.resident {
                        let cloned_page = SharedResident::new(source.page.clone(), source.writer);
                        if MapArea::has_leaf_permission(area.map_permission) {
                            cloned.page_table.map(
                                vpn,
                                cloned_page.page.frame().ppn(),
                                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
                            )?;
                        }
                        resident.insert(vpn, cloned_page);
                    }
                    for vpn in area.vpn_range.start.as_usize()..area.vpn_range.end.as_usize() {
                        let vpn = VirtualPageNumber::from_vpn(vpn);
                        if !resident.contains_key(&vpn) {
                            cloned.page_table.reserve(vpn)?;
                        }
                    }
                    let cloned_area = MapArea {
                        vpn_range: area.vpn_range.clone(),
                        data_page_offset: area.data_page_offset,
                        data_frames: BTreeMap::new(),
                        map_type: area.map_type,
                        map_permission: area.map_permission,
                        global: area.global,
                        kind: area.kind,
                        shared_file: Some(SharedFileArea {
                            mapping: shared.mapping.clone(),
                            file_offset: shared.file_offset,
                            resident,
                        }),
                    };
                    assert!(cloned.areas.insert(*key, cloned_area).is_none());
                    continue;
                }
                let cloned_area = MapArea {
                    vpn_range: area.vpn_range.clone(),
                    data_page_offset: area.data_page_offset,
                    data_frames: area.data_frames.clone(),
                    map_type: area.map_type,
                    map_permission: area.map_permission,
                    global: area.global,
                    kind: area.kind,
                    shared_file: None,
                };
                if MapArea::has_leaf_permission(area.map_permission) {
                    let mut flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
                    flags.remove(PTEFlags::W);
                    for (&vpn, frame) in &area.data_frames {
                        if area.map_permission.contains(MapPermission::W) {
                            self.page_table.set_flags(vpn, flags)?;
                        }
                        cloned.page_table.map(vpn, frame.ppn, flags)?;
                    }
                } else {
                    for &vpn in area.data_frames.keys() {
                        cloned.page_table.reserve(vpn)?;
                    }
                }
                cloned_area
            } else {
                area.try_clone_into(&mut cloned.page_table)?
            };
            assert!(cloned.areas.insert(*key, cloned_area).is_none());
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after fork COW publication");
        Ok(cloned)
    }

    /// @description 处理可写用户 VMA 上的 COW store fault。
    pub(crate) fn handle_cow_fault(&mut self, address: usize) -> Result<bool, MemoryError> {
        let vpn = VirtualAddress::from(address).floor();
        let Some((_, area)) = self.areas.range_mut(..=vpn).next_back() else {
            return Ok(false);
        };
        if vpn >= area.vpn_range.end
            || !area
                .map_permission
                .contains(MapPermission::U | MapPermission::W)
            || area.map_type != MapType::Framed
        {
            return Ok(false);
        }
        let frame = area
            .data_frames
            .get_mut(&vpn)
            .ok_or(MemoryError::InvalidRange)?;
        if self
            .page_table
            .translate(vpn)
            .is_some_and(|pte| pte.flags().contains(PTEFlags::W))
        {
            return Ok(true);
        }
        if Arc::strong_count(frame) > 1 {
            let mut replacement = alloc().ok_or(MemoryError::OutOfMemory)?;
            replacement.bytes_mut().copy_from_slice(frame.bytes());
            *frame = Arc::new(replacement);
            self.page_table.unmap(vpn)?;
            self.page_table.map(
                vpn,
                frame.ppn,
                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
            )?;
        } else {
            self.page_table.set_flags(
                vpn,
                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
            )?;
        }
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after COW resolution");
        Ok(true)
    }

    pub(super) fn prepare_user_write(
        &mut self,
        address: usize,
        length: usize,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(address, length)?;
        let mut current = address;
        while current < end {
            match self.handle_page_fault(current, PageFaultAccess::Write) {
                Ok(PageFaultOutcome::Handled) => {}
                Err(MemoryError::OutOfMemory) => return Err(UserAccessError::OutOfMemory),
                _ => return Err(UserAccessError::Fault),
            }
            current = (current | (config::PAGE_SIZE - 1))
                .saturating_add(1)
                .min(end);
        }
        self.validate_user_range(address, length, PTEFlags::W)
    }
}
