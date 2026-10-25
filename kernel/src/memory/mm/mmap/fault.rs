use super::*;

impl MemorySet {
    fn complete_stale_translation_fault(vpn: VirtualPageNumber) {
        TranslationCommit::stale_fault(vpn.as_usize())
            .synchronize()
            .expect("local translation fence failed while resolving a stale page fault");
    }

    fn classify_page_fault(
        &self,
        vpn: VirtualPageNumber,
        access: PageFaultAccess,
    ) -> Result<FaultPreflight, MemoryError> {
        let Some((_, area)) = self.areas.floor(&vpn) else {
            return preflight_fault(
                false,
                FaultPermissions::new(false, false, false, false),
                access,
                || Ok(FileFaultState::NotFile),
                || FaultResidency::Private {
                    lazy: false,
                    resident: false,
                },
            );
        };
        let contains = vpn < area.vpn_range.end;
        let permissions = FaultPermissions::new(
            area.map_permission.contains(MapPermission::U),
            area.map_permission.contains(MapPermission::R),
            area.map_permission.contains(MapPermission::W),
            area.map_permission.contains(MapPermission::X),
        );
        preflight_fault(
            contains,
            permissions,
            access,
            || {
                Ok(match &area.private_file {
                    Some(backing) => match backing.faultable(vpn)? {
                        true => FileFaultState::Available,
                        false => FileFaultState::BeyondEof,
                    },
                    None => FileFaultState::NotFile,
                })
            },
            || {
                if area.device.is_some() {
                    FaultResidency::Device
                } else if area.shared_anonymous.is_some() {
                    FaultResidency::SharedAnonymous
                } else if area.shared_file.is_some() {
                    FaultResidency::SharedFile
                } else {
                    FaultResidency::Private {
                        lazy: area.lazy_private,
                        resident: area.data_frames.contains_key(&vpn),
                    }
                }
            },
        )
    }

    fn grow_stack_for_fault(
        &mut self,
        address: usize,
        stack_limit: u64,
        address_space_limit: u64,
    ) -> Result<(), MemoryError> {
        let target = VirtualAddress::from(address).floor();
        // 与 Linux find_vma/expand_downwards 相同，只考虑地址上方最近的 VMA。
        // split 可留下多个 VM_GROWSDOWN fragments；不存在“唯一原始 stack”的另一路 cache。
        let Some((&key, area)) = self.areas.ceiling(&target) else {
            return Ok(());
        };
        if area.kind != VmaKind::Stack || target >= key {
            return Ok(());
        }
        let top = usize::from(VirtualAddress::from(area.vpn_range.end));
        let target_address = target.as_usize() * config::PAGE_SIZE;
        let stack_limit = usize::try_from(stack_limit).unwrap_or(usize::MAX);
        if target_address < top.saturating_sub(stack_limit) {
            return Ok(());
        }
        if target.as_usize() == 0
            || self.areas.predecessor(&key).is_some_and(|(_, previous)| {
                let previous_end = previous.vpn_range.end.as_usize();
                // Linux stack_guard_gap 默认 256 pages；两个 grow-down fragments 之间无需该 gap。
                // 已映射地址绝不通过 growth 覆盖，PROT_NONE 邻居也仍拥有其真实 range。
                previous_end > target.as_usize()
                    || previous.kind != VmaKind::Stack
                        && MapArea::has_leaf_permission(previous.map_permission)
                        && target.as_usize() - previous_end < 256
            })
        {
            return Ok(());
        }
        let additional = (key.as_usize() - target.as_usize()) as u64 * config::PAGE_SIZE as u64;
        self.ensure_resource_capacity(additional, address_space_limit, None)?;
        // 复用原 VMA node 完成 rekey；若重新分配，stack fault 会在无物理页需求时
        // 仍可能因 metadata OOM 失败，并迫使 caller 维护回滚分支。
        let mut area = self
            .take_area_entry(&key)
            .expect("stack key must remain live");
        area.value_mut().vpn_range.start = target;
        area.set_key(target);
        self.commit_area(area);
        Ok(())
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
        let preflight = self.classify_page_fault(vpn, access)?;
        let prepared_private_file = if preflight == FaultPreflight::NeedsPrivateFrame {
            let (_, area) = self.areas.floor(&vpn).ok_or(MemoryError::InvalidRange)?;
            area.private_file
                .as_ref()
                .map(|backing| backing.prepare_fault(vpn))
                .transpose()?
        } else {
            None
        };
        if matches!(
            prepared_private_file.as_ref(),
            Some(PrivateFaultPreparation::BeyondEof)
        ) {
            return Ok(PageFaultOutcome::BusError);
        }
        // Linux do_read_fault 保留文件页；提前复制会让只读 MAP_PRIVATE 永久看不到后续文件写入。
        let share_file_page = access != PageFaultAccess::Write
            && matches!(
                prepared_private_file,
                Some(PrivateFaultPreparation::Cached(_))
            );
        let mut prepared_private_frame = match preflight {
            FaultPreflight::SegmentationFault(cause) => {
                return Ok(PageFaultOutcome::SegmentationFault(cause));
            }
            FaultPreflight::BusError => return Ok(PageFaultOutcome::BusError),
            FaultPreflight::Device
            | FaultPreflight::SharedAnonymous
            | FaultPreflight::SharedFile
            | FaultPreflight::Private => None,
            FaultPreflight::NeedsPrivateFrame if !share_file_page => {
                Some(self.allocate_private_frame()?)
            }
            FaultPreflight::NeedsPrivateFrame => None,
        };
        if prepared_private_frame.is_some() {
            let still_missing = self.areas.floor(&vpn).is_some_and(|(_, area)| {
                vpn < area.vpn_range.end
                    && area.lazy_private
                    && area.shared_anonymous.is_none()
                    && area.shared_file.is_none()
                    && area.device.is_none()
                    && !area.data_frames.contains_key(&vpn)
            });
            if !still_missing {
                return Err(MemoryError::InvalidRange);
            }
        }
        let Some((_, area)) = self.areas.floor_mut(&vpn) else {
            return Err(MemoryError::InvalidRange);
        };
        debug_assert!(vpn < area.vpn_range.end);
        if area.device.is_some() {
            return Ok(match self.page_table.translate(vpn) {
                Some(_) => {
                    Self::complete_stale_translation_fault(vpn);
                    PageFaultOutcome::Handled
                }
                None => PageFaultOutcome::SegmentationFault(SegmentationCause::AccessDenied),
            });
        }
        if let Some(shared) = &area.shared_anonymous {
            if !area.data_frames.contains_key(&vpn) {
                let index = shared.page_offset
                    + vpn
                        .as_usize()
                        .saturating_sub(area.vpn_range.start.as_usize());
                // mremap 增长出来的页没有 backing：Linux 的 shmem 文件没有同步扩容，访问返回 SIGBUS。
                if index >= shared.backing.page_count {
                    return Ok(PageFaultOutcome::BusError);
                }
                let frame = shared.backing.page(index)?;
                let pin = frame.pin()?;
                let ppn = pin.ppn();
                let resident = area
                    .data_frames
                    .try_prepare_vacant(vpn, PrivateResident::shared(&frame, &pin, false))
                    .map_err(|_| MemoryError::OutOfMemory)?;
                let mut commit = TranslationCommit::new();
                self.page_table
                    .map(vpn, ppn, area.map_permission.into(), &mut commit)?;
                area.data_frames.commit_vacant(resident);
                commit
                    .synchronize()
                    .expect("local translation fence failed after shared anonymous page fault");
                return Ok(PageFaultOutcome::Handled);
            }
            let resident = area
                .data_frames
                .get_mut(&vpn)
                .expect("resident shared page");
            let page = resident
                .mapping()
                .expect("shared anonymous canonical mapping")
                .page()
                .clone();
            let pin = page.pin()?;
            if self.page_table.translate(vpn).is_none() {
                let mut commit = TranslationCommit::new();
                self.page_table
                    .map(vpn, pin.ppn(), area.map_permission.into(), &mut commit)?;
                commit.synchronize().expect(
                    "local translation fence failed after shared anonymous permission fault",
                );
            } else {
                Self::complete_stale_translation_fault(vpn);
            }
            if let PrivateBacking::User(mapping) = &mut resident.backing {
                mapping.mark_installed(&pin);
            }
            return Ok(PageFaultOutcome::Handled);
        }
        if area.shared_file.is_none() {
            if area.lazy_private && !area.data_frames.contains_key(&vpn) {
                let file_pin = if share_file_page {
                    match &prepared_private_file {
                        Some(PrivateFaultPreparation::Cached(page)) => Some(page.pin()?),
                        _ => unreachable!("shared private fault requires a file page"),
                    }
                } else {
                    None
                };
                let (mut resident, ppn) = if let Some(pin) = &file_pin {
                    let Some(PrivateFaultPreparation::Cached(page)) = &prepared_private_file else {
                        unreachable!("pinned private fault requires a file page");
                    };
                    (PrivateResident::file(page, pin), pin.ppn())
                } else {
                    let mut frame = prepared_private_frame
                        .take()
                        .ok_or(MemoryError::OutOfMemory)?;
                    if let Some(backing) = &area.private_file {
                        let prepared = prepared_private_file
                            .as_ref()
                            .ok_or(MemoryError::InvalidRange)?;
                        backing.fill(vpn, &mut frame, prepared)?;
                    }
                    let ppn = frame.ppn;
                    (PrivateResident::user(frame)?, ppn)
                };
                let mut flags: PagePermissions = area.map_permission.into();
                if area.private_file.is_some() && area.map_permission.contains(MapPermission::W) {
                    // 首次 read 保持只读，后续 store fault 是标记 MAP_PRIVATE dirty 的唯一入口。
                    flags.remove(PagePermissions::WRITE);
                    if access == PageFaultAccess::Write {
                        resident.dirty = true;
                        flags |= PagePermissions::WRITE;
                    }
                }
                let resident = area
                    .data_frames
                    .try_prepare_vacant(vpn, resident)
                    .map_err(|_| MemoryError::OutOfMemory)?;
                let mut commit = TranslationCommit::new();
                self.page_table.map(vpn, ppn, flags, &mut commit)?;
                area.data_frames.commit_vacant(resident);
                commit
                    .synchronize()
                    .expect("local translation fence failed after private page fault");
                return Ok(PageFaultOutcome::Handled);
            }
            // 回收达到 target 后可保留 resident 而撤销 PTE；read/execute/write
            // 都从同一 owner 重建，并保持 COW、MADV_FREE 与 private-file dirty 语义。
            return Ok(if self.handle_private_resident_fault(address, access)? {
                PageFaultOutcome::Handled
            } else {
                PageFaultOutcome::SegmentationFault(SegmentationCause::AccessDenied)
            });
        }
        let shared = area.shared_file.as_mut().unwrap();
        let permission = shared.leaf_permission(area.map_permission);
        if access == PageFaultAccess::Write && !permission.contains(MapPermission::W) {
            return Ok(PageFaultOutcome::BusError);
        }
        let page = shared
            .page(area.vpn_range.start, vpn)
            .ok_or(MemoryError::InvalidRange)?;
        let prepared_page = match shared.mapping.page(page) {
            Ok(page) => page,
            Err(SharedFileError::BeyondEof) => return Ok(PageFaultOutcome::BusError),
            Err(SharedFileError::OutOfMemory) => return Err(MemoryError::OutOfMemory),
            Err(SharedFileError::Io) => return Err(MemoryError::Io),
        };
        if let Some(resident) = shared.resident.get_mut(&vpn) {
            let page = resident.page.clone();
            let pin = page.pin()?;
            let writer = permission.contains(MapPermission::W);
            if writer && !resident.writer {
                resident.page.acquire_writer();
                resident.writer = true;
            }
            let flags: PagePermissions = permission.into();
            let mut commit = TranslationCommit::new();
            match self.page_table.translate(vpn) {
                None => self.page_table.map(vpn, pin.ppn(), flags, &mut commit)?,
                Some(entry) if entry.permissions() != flags => {
                    self.page_table.set_flags(vpn, flags, &mut commit)?;
                }
                Some(_) => {
                    // leaf 已满足访问时 set_flags 不记录 fence；刷新旧翻译才能重试原指令。
                    commit = TranslationCommit::stale_fault(vpn.as_usize());
                }
            }
            commit
                .synchronize()
                .expect("local translation fence failed after shared file permission fault");
            resident.mark_installed(&pin);
            return Ok(PageFaultOutcome::Handled);
        }
        let pin = prepared_page.pin()?;
        let resident =
            SharedResident::new(&prepared_page, permission.contains(MapPermission::W), &pin);
        let ppn = pin.ppn();
        let resident = shared
            .resident
            .try_prepare_vacant(vpn, resident)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let flags = permission.into();
        let mut commit = TranslationCommit::new();
        self.page_table.map(vpn, ppn, flags, &mut commit)?;
        shared.resident.commit_vacant(resident);
        commit
            .synchronize()
            .expect("local translation fence failed after shared page fault");
        Ok(PageFaultOutcome::Handled)
    }
}
