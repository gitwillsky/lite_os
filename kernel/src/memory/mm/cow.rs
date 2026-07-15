use super::*;

impl MapArea {
    fn try_clone_into(&self, page_table: &mut PageTable) -> Result<Self, MemoryError> {
        let mut cloned = Self {
            vpn_range: self.vpn_range.clone(),
            data_page_offset: self.data_page_offset,
            data_frames: FallibleMap::new(),
            map_type: self.map_type,
            map_permission: self.map_permission,
            global: self.global,
            kind: self.kind,
            shared_anonymous: None,
            shared_file: None,
            device: None,
            private_file: self.private_file.clone(),
            lazy_private: self.lazy_private,
        };
        if let Err(error) = cloned.map(page_table) {
            cloned.unmap(page_table);
            return Err(error);
        }
        for (vpn, source) in &self.data_frames {
            Arc::get_mut(
                cloned
                    .data_frames
                    .get_mut(vpn)
                    .expect("cloned framed VMA must own every source VPN"),
            )
            .expect("eager-cloned system frame must be unique")
            .bytes_mut()
            .copy_from_slice(source.bytes());
        }
        Ok(cloned)
    }

    fn try_clone_data_frames(
        &self,
    ) -> Result<FallibleMap<VirtualPageNumber, PrivateResident>, MemoryError> {
        let mut cloned = FallibleMap::new();
        for (&vpn, resident) in &self.data_frames {
            cloned
                .try_insert(vpn, resident.clone())
                .map_err(|_| MemoryError::OutOfMemory)?;
        }
        Ok(cloned)
    }
}

fn clone_shared_file_area(
    area: &MapArea,
    page_table: &mut PageTable,
) -> Result<MapArea, MemoryError> {
    let shared = area
        .shared_file
        .as_ref()
        .expect("shared-file clone requires shared metadata");
    let mut resident = FallibleMap::new();
    for (&vpn, source) in &shared.resident {
        let cloned_page = SharedResident::new(source.page.clone(), source.writer);
        let ppn = cloned_page.page.frame().ppn();
        let prepared = resident
            .try_prepare_vacant(vpn, cloned_page)
            .map_err(|_| MemoryError::OutOfMemory)?;
        if MapArea::has_leaf_permission(area.map_permission) {
            page_table.map(
                vpn,
                ppn,
                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
            )?;
        }
        resident.commit_vacant(prepared);
    }
    Ok(MapArea {
        vpn_range: area.vpn_range.clone(),
        data_page_offset: area.data_page_offset,
        data_frames: FallibleMap::new(),
        map_type: area.map_type,
        map_permission: area.map_permission,
        global: area.global,
        kind: area.kind,
        shared_anonymous: None,
        shared_file: Some(SharedFileArea {
            mapping: shared.mapping.clone(),
            pages: shared.pages,
            resident,
        }),
        device: None,
        private_file: None,
        lazy_private: false,
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
        let page_table = &mut self.page_table;
        self.areas.try_for_each_mut(|key, area| {
            // 先预留 cloned VMA node；缺失它会在 parent COW PTE 与 child PTE
            // 已改变后才发现无法发布 child VMA owner。
            let area_slot =
                FallibleMap::try_reserve_node().map_err(|_| MemoryError::OutOfMemory)?;
            let cloned_area = if area.map_permission.contains(MapPermission::U) {
                if area.device.is_some() {
                    area.try_clone_device_into(&mut cloned.page_table)?
                } else if area.shared_file.is_some() {
                    clone_shared_file_area(area, &mut cloned.page_table)?
                } else {
                    let cloned_area = MapArea {
                        vpn_range: area.vpn_range.clone(),
                        data_page_offset: area.data_page_offset,
                        data_frames: area.try_clone_data_frames()?,
                        map_type: area.map_type,
                        map_permission: area.map_permission,
                        global: area.global,
                        kind: area.kind,
                        shared_anonymous: area.shared_anonymous.clone(),
                        shared_file: None,
                        device: None,
                        private_file: area.private_file.clone(),
                        lazy_private: area.lazy_private,
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
                                page_table.set_flags(vpn, flags)?;
                            }
                            cloned.page_table.map(vpn, frame.ppn, flags)?;
                        }
                    } else {
                        for (&vpn, _) in &area.data_frames {
                            cloned.page_table.reserve(vpn)?;
                        }
                    }
                    cloned_area
                }
            } else {
                area.try_clone_into(&mut cloned.page_table)?
            };
            cloned
                .areas
                .commit_vacant(area_slot.fill(*key, cloned_area));
            Ok::<(), MemoryError>(())
        })?;
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after fork COW publication");
        Ok(cloned)
    }

    /// @description 处理可写用户 VMA 上的 COW store fault。
    pub(crate) fn handle_cow_fault(&mut self, address: usize) -> Result<bool, MemoryError> {
        let vpn = VirtualAddress::from(address).floor();
        let Some((_, area)) = self.areas.floor_mut(&vpn) else {
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
            let replacement = try_memory_arc(replacement)?;
            self.page_table.unmap(vpn)?;
            self.page_table.map(
                vpn,
                replacement.ppn,
                PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
            )?;
            frame.frame = replacement;
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
