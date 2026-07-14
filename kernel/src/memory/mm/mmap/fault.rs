use super::*;

impl MemorySet {
    fn grow_stack_for_fault(
        &mut self,
        address: usize,
        stack_limit: u64,
        address_space_limit: u64,
    ) -> Result<(), MemoryError> {
        let target = VirtualAddress::from(address).floor();
        let Some((key, top)) = self.areas.iter().find_map(|(key, area)| match area.kind {
            VmaKind::Stack { top } => Some((*key, top)),
            _ => None,
        }) else {
            return Ok(());
        };
        if target >= key {
            return Ok(());
        }
        let target_address = target.as_usize() * config::PAGE_SIZE;
        let stack_limit = usize::try_from(stack_limit).unwrap_or(usize::MAX);
        if target_address < top.saturating_sub(stack_limit) {
            return Ok(());
        }
        if self.areas.predecessor(&key).is_some_and(|(_, previous)| {
            previous.vpn_range.end.as_usize().saturating_add(1) > target.as_usize()
        }) {
            return Ok(());
        }
        let additional = (key.as_usize() - target.as_usize()) as u64 * config::PAGE_SIZE as u64;
        self.ensure_resource_capacity(additional, address_space_limit, None)?;
        // 复用原 VMA node 完成 rekey；若重新分配，stack fault 会在无物理页需求时
        // 仍可能因 metadata OOM 失败，并迫使 caller 维护回滚分支。
        let mut area = self
            .areas
            .take_entry(&key)
            .expect("stack key must remain live");
        area.value_mut().vpn_range.start = target;
        area.set_key(target);
        self.areas.commit_vacant(area);
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
        let needs_private_frame = self
            .areas
            .floor(&vpn)
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
        let Some((_, area)) = self.areas.floor_mut(&vpn) else {
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
        if area.device.is_some() {
            return Ok(if self.page_table.translate(vpn).is_some() {
                PageFaultOutcome::Handled
            } else {
                PageFaultOutcome::SegmentationFault
            });
        }
        if let Some(shared) = &area.shared_anonymous {
            if !area.data_frames.contains_key(&vpn) {
                let index = shared.page_offset
                    + vpn
                        .as_usize()
                        .saturating_sub(area.vpn_range.start.as_usize());
                let frame = shared.backing.page(index)?;
                let ppn = frame.ppn;
                let resident = area
                    .data_frames
                    .try_prepare_vacant(vpn, PrivateResident::new(frame))
                    .map_err(|_| MemoryError::OutOfMemory)?;
                self.page_table.map(
                    vpn,
                    ppn,
                    PTEFlags::from_bits(area.map_permission.bits()).unwrap(),
                )?;
                area.data_frames.commit_vacant(resident);
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
            if access == PageFaultAccess::Write
                && area
                    .data_frames
                    .get_mut(&vpn)
                    .is_some_and(|resident| core::mem::take(&mut resident.discardable))
            {
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
                let frame = try_memory_arc(frame)?;
                let mut resident = PrivateResident::new(frame);
                let mut flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
                if area.private_file.is_some() && area.map_permission.contains(MapPermission::W) {
                    // 首次 read 保持只读，后续 store fault 是标记 MAP_PRIVATE dirty 的唯一入口。
                    flags.remove(PTEFlags::W);
                    if access == PageFaultAccess::Write {
                        resident.dirty = true;
                        flags |= PTEFlags::W;
                    }
                }
                let resident = area
                    .data_frames
                    .try_prepare_vacant(vpn, resident)
                    .map_err(|_| MemoryError::OutOfMemory)?;
                self.page_table.map(vpn, ppn, flags)?;
                area.data_frames.commit_vacant(resident);
                Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after private page fault");
                return Ok(PageFaultOutcome::Handled);
            }
            if access == PageFaultAccess::Write && area.private_file.is_some() {
                area.data_frames
                    .get_mut(&vpn)
                    .expect("private file page fault lost resident page")
                    .dirty = true;
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
        let ppn = resident.page.frame().ppn();
        let resident = shared
            .resident
            .try_prepare_vacant(vpn, resident)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let flags = PTEFlags::from_bits(area.map_permission.bits()).unwrap();
        self.page_table.map(vpn, ppn, flags)?;
        shared.resident.commit_vacant(resident);
        Self::flush_tlb_all_cpus().expect("SBI RFENCE failed after shared page fault");
        Ok(PageFaultOutcome::Handled)
    }

    fn allocate_private_frame(&mut self) -> Result<FrameTracker, MemoryError> {
        if let Some(frame) = alloc() {
            return Ok(frame);
        }
        // 1. 当前 mm 在已持有 AddressSpace lock 下直接回收；registry 会通过 try_lock 跳过它。
        let _ = self.reclaim_private_pages(ReclaimRequest::for_target(64));
        // 2. alloc 的统一慢路径会在需要时再请求其他 resident owner，最后只重试一次。
        alloc().ok_or(MemoryError::OutOfMemory)
    }
}
