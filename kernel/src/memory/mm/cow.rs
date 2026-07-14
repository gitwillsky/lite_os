use super::*;

fn clone_shared_file_area(
    area: &MapArea,
    page_table: &mut PageTable,
) -> Result<MapArea, MemoryError> {
    let shared = area
        .shared_file
        .as_ref()
        .expect("shared-file clone requires shared metadata");
    let mut resident = BTreeMap::new();
    for (&vpn, source) in &shared.resident {
        let cloned_page = SharedResident::new(source.page.clone(), source.writer);
        if MapArea::has_leaf_permission(area.map_permission) {
            page_table.map(
                vpn,
                cloned_page.page.frame().ppn(),
                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
            )?;
        }
        resident.insert(vpn, cloned_page);
    }
    Ok(MapArea {
        vpn_range: area.vpn_range.clone(),
        data_page_offset: area.data_page_offset,
        data_frames: BTreeMap::new(),
        map_type: area.map_type,
        map_permission: area.map_permission,
        global: area.global,
        kind: area.kind,
        shared_anonymous: None,
        shared_file: Some(SharedFileArea {
            mapping: shared.mapping.clone(),
            file_offset: shared.file_offset,
            resident,
        }),
        private_file: None,
        lazy_private: false,
        discardable: BTreeSet::new(),
        dirty_private: BTreeSet::new(),
    })
}

impl MemorySet {
    /// @description 为 fork 共享用户 frame 并把可写映射转换为 COW；supervisor frame 仍独立复制。
    pub(crate) fn try_clone_for_fork(&mut self) -> Result<Self, MemoryError> {
        let mut cloned = Self::try_new()?;
        cloned.code_range = self.code_range.clone();
        cloned.program_break = self.program_break;
        cloned.argument_range = self.argument_range.clone();
        cloned.map_trampoline()?;
        for (key, area) in &mut self.areas {
            let cloned_area = if area.map_permission.contains(MapPermission::U) {
                if area.shared_file.is_some() {
                    let cloned_area = clone_shared_file_area(area, &mut cloned.page_table)?;
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
                    shared_anonymous: area.shared_anonymous.clone(),
                    shared_file: None,
                    private_file: area.private_file.clone(),
                    lazy_private: area.lazy_private,
                    discardable: area.discardable.clone(),
                    dirty_private: area.dirty_private.clone(),
                };
                if MapArea::has_leaf_permission(area.map_permission) {
                    let mut flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
                    if area.shared_anonymous.is_none() {
                        flags.remove(PTEFlags::W);
                    }
                    for (&vpn, frame) in &area.data_frames {
                        if area.map_permission.contains(MapPermission::W)
                            && area.shared_anonymous.is_none()
                        {
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
        if area.shared_anonymous.is_some() {
            return Ok(self
                .page_table
                .translate(vpn)
                .is_some_and(|pte| pte.flags().contains(PTEFlags::W)));
        }
        let Some(frame) = area.data_frames.get_mut(&vpn) else {
            return Ok(false);
        };
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
}
