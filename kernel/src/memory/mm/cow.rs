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
                    .expect("cloned framed VMA must own every source VPN")
                    .pinned_frame_mut(),
            )
            .expect("eager-cloned system frame must be unique")
            .bytes_mut()
            .copy_from_slice(source.pinned_frame().bytes());
        }
        Ok(cloned)
    }

    fn try_clone_data_frames(
        &self,
    ) -> Result<FallibleMap<VirtualPageNumber, PrivateResident>, MemoryError> {
        let mut cloned = FallibleMap::new();
        for (&vpn, resident) in &self.data_frames {
            // truncate 已公布长度但仍在等待 parent mm 时，parent 可暂留旧 EOF resident。
            // 晚登记 child 不一定在该次扫描内；首次继承必须服从已公布的 backing EOF。
            if let Some(backing) = &self.private_file
                && !backing.faultable(vpn)?
            {
                continue;
            }
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
    let file_size = shared.mapping.size();
    let leaf_permission = shared.leaf_permission(area.map_permission);
    for (&vpn, source) in &shared.resident {
        let relative = vpn.as_usize() - area.vpn_range.start.as_usize();
        if !shared
            .pages
            .has_file_bytes(relative as u64, file_size)
            .ok_or(MemoryError::InvalidRange)?
        {
            continue;
        }
        let pin = source.pin_installed()?;
        let leaf = pin.is_some() && MapArea::has_leaf_permission(leaf_permission);
        let cloned_page = source.inherit(leaf && leaf_permission.contains(MapPermission::W));
        let prepared = resident
            .try_prepare_vacant(vpn, cloned_page)
            .map_err(|_| MemoryError::OutOfMemory)?;
        if let Some(pin) = pin.filter(|_| leaf) {
            page_table.map(vpn, pin.ppn(), leaf_permission.into(), commit)?;
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
    /// 为 fork 共享用户 frame 并把可写映射转换为 COW；supervisor frame 仍独立复制。
    /// child 必须已在 live mapping/reclaim registry 登记，但尚未创建 Thread 或激活。
    /// 调用者同时独占两个 mm；root/ASID 不替换，失败由 child owner 完整 retirement。
    pub(crate) fn try_clone_for_fork(&mut self, cloned: &mut Self) -> Result<(), MemoryError> {
        assert!(
            cloned.areas.is_empty(),
            "fork child must begin with no VMA owners"
        );
        assert!(
            cloned.executable_owner.is_none(),
            "fork child executable owner already installed"
        );
        cloned.executable_owner = self.executable_owner.clone();
        cloned.code_range = self.code_range.clone();
        cloned.program_break = self.program_break;
        cloned.argument_range = self.argument_range.clone();
        cloned.map_trampoline()?;
        let page_table = &mut self.page_table;
        let mut parent_commit = TranslationCommit::new();
        let mut child_commit = TranslationCommit::new();
        let result = self.areas.try_for_each_mut(|key, area| {
            // child 只创建 calling Thread；其他 Thread 的 temporary context 不归 child MM。
            // canonical context 单独保留，task 随后写入 spawning Thread 的实际寄存器快照。
            if crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE
                .as_ref()
                .is_some_and(|range| {
                    range.contains(&usize::from(VirtualAddress::from(area.vpn_range.start)))
                })
            {
                return Ok(());
            }
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
                        for (&vpn, resident) in &cloned_area.data_frames {
                            let Some(frame) = resident.pin_resident()? else {
                                continue;
                            };
                            if area.map_permission.contains(MapPermission::W)
                                && area.shared_anonymous.is_none()
                                && page_table.translate(vpn).is_some()
                            {
                                page_table.set_flags(vpn, flags, &mut parent_commit)?;
                            }
                            cloned
                                .page_table
                                .map(vpn, frame.ppn(), flags, &mut child_commit)?;
                        }
                    } else {
                        for (&vpn, _) in &cloned_area.data_frames {
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
        Ok(())
    }
}
