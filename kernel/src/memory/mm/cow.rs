use super::*;

impl MapArea {
    fn try_clone_into(
        &self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) -> Result<Self, MemoryError> {
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
        if let Err(error) = cloned.map(page_table, commit) {
            // Child page table 尚未发布或激活；撤销 translation 后可直接 Drop owner，
            // 不存在 remote stale translation retire 窗口。
            let mut rollback = TranslationCommit::new();
            cloned.unmap(page_table, &mut rollback);
            rollback.finish_unpublished();
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
    commit: &mut TranslationCommit,
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
            page_table.map(vpn, ppn, area.map_permission.into(), commit)?;
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
        let mut parent_commit = TranslationCommit::new();
        let mut child_commit = TranslationCommit::new();
        let result = self.areas.try_for_each_mut(|key, area| {
            // 先预留 cloned VMA node；缺失它会在 parent COW PTE 与 child PTE
            // 已改变后才发现无法发布 child VMA owner。
            let area_slot =
                FallibleMap::try_reserve_node().map_err(|_| MemoryError::OutOfMemory)?;
            let cloned_area = if area.map_permission.contains(MapPermission::U) {
                if area.device.is_some() {
                    area.try_clone_device_into(&mut cloned.page_table, &mut child_commit)?
                } else if area.shared_file.is_some() {
                    clone_shared_file_area(area, &mut cloned.page_table, &mut child_commit)?
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
                        let mut flags: PagePermissions = area.map_permission.into();
                        if area.shared_anonymous.is_none() {
                            flags.remove(PagePermissions::WRITE);
                        }
                        for (&vpn, frame) in &area.data_frames {
                            if area.map_permission.contains(MapPermission::W)
                                && area.shared_anonymous.is_none()
                            {
                                page_table.set_flags(vpn, flags, &mut parent_commit)?;
                            }
                            cloned
                                .page_table
                                .map(vpn, frame.ppn, flags, &mut child_commit)?;
                        }
                    } else {
                        for (&vpn, _) in &area.data_frames {
                            cloned.page_table.reserve(vpn)?;
                        }
                    }
                    cloned_area
                }
            } else {
                area.try_clone_into(&mut cloned.page_table, &mut child_commit)?
            };
            cloned.commit_area(area_slot.fill(*key, cloned_area));
            Ok::<(), MemoryError>(())
        });
        child_commit.finish_unpublished();
        parent_commit
            .synchronize()
            .expect("platform translation fence failed after fork COW restriction");
        result?;
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
            let writable = self
                .page_table
                .translate(vpn)
                .is_some_and(|pte| pte.permissions().contains(PagePermissions::WRITE));
            if writable {
                TranslationCommit::stale_fault(vpn.as_usize())
                    .synchronize()
                    .expect("local translation fence failed after shared write fault");
            }
            return Ok(writable);
        }
        let Some(frame) = area.data_frames.get_mut(&vpn) else {
            return Ok(false);
        };
        if self
            .page_table
            .translate(vpn)
            .is_some_and(|pte| pte.permissions().contains(PagePermissions::WRITE))
        {
            TranslationCommit::stale_fault(vpn.as_usize())
                .synchronize()
                .expect("local translation fence failed after stale COW write fault");
            return Ok(true);
        }
        let mut commit = TranslationCommit::new();
        if Arc::strong_count(frame) > 1 {
            let replacement = alloc_copy(frame.bytes()).ok_or(MemoryError::OutOfMemory)?;
            let replacement = try_memory_arc(replacement)?;
            self.page_table.unmap(vpn, &mut commit)?;
            self.page_table.map(
                vpn,
                replacement.ppn,
                area.map_permission.into(),
                &mut commit,
            )?;
            let retired = core::mem::replace(&mut frame.frame, replacement);
            let retired = revoke_and_synchronize(retired, |_| {}, |_| commit.synchronize())
                .expect("platform translation fence failed after COW frame replacement");
            drop(retired);
        } else {
            self.page_table
                .set_flags(vpn, area.map_permission.into(), &mut commit)?;
            commit
                .synchronize()
                .expect("local translation fence failed after COW permission increase");
        }
        Ok(true)
    }
}
