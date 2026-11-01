use super::*;

impl MemorySet {
    /// 建立 eager anonymous shared mapping；backing 是所有 fork descendant 页帧
    /// 与 futex identity 的唯一 owner。
    ///
    /// # Parameters
    ///
    /// - `address`: 零表示由内核选址；非零是 hint 或 exact 地址。
    /// - `length`: 非零字节长度，向上取整到整页。
    /// - `permission`: 用户页权限；允许 PROT_NONE 与 Linux W+X 映射。
    /// - `fixed_noreplace`: 为真时必须精确使用 address，冲突不替换。
    ///
    /// # Returns
    ///
    /// 成功返回映射起始地址；分配或页表提交失败不留下 VMA。
    pub(crate) fn map_shared_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.map_anonymous_shared_source(
            address,
            length,
            permission,
            fixed_noreplace,
            address_space_limit,
            None,
        )
    }

    /// 使用 IPC 已授权的实际共享 backing；计数只由新 MapArea 持有。
    pub(crate) fn map_system_v_shared_memory(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        source: crate::memory::SharedMemoryMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        self.map_anonymous_shared_source(
            address,
            length,
            permission,
            address != 0,
            address_space_limit,
            Some(source),
        )
    }

    fn map_anonymous_shared_source(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
        source: Option<crate::memory::SharedMemoryMappingSource>,
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
        let shared = if let Some(source) = source {
            if source.backing.page_count != page_count
                || permission.contains(MapPermission::W) && !source.may_write
            {
                return Err(MemoryError::PermissionDenied);
            }
            SharedAnonymousArea {
                backing: source.backing.clone(),
                page_offset: 0,
                sysv: Some(crate::memory::shared_memory::SharedMemoryVmaMembership::new(source)),
            }
        } else {
            SharedAnonymousArea {
                backing: AnonymousSharedBacking::allocate(page_count)?,
                page_offset: 0,
                sysv: None,
            }
        };
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::shared_anonymous(start.into(), end.into(), permission, shared),
            None,
        )?;
        Ok(start)
    }

    /// shmdt 只摘除同一次 shmat 的实际 VMA fragments，保留期间覆盖的其他映射。
    /// 原起点的一部分已 munmap 时仍可由 page_offset 找到余下 fragments。
    pub(crate) fn prepare_shared_memory_detach(
        &mut self,
        address: usize,
    ) -> Result<PreparedAreaRetirement, MemoryError> {
        if !VirtualAddress::from(address).is_aligned() || address >= config::USER_ADDRESS_END {
            return Err(MemoryError::InvalidRange);
        }
        let start = VirtualAddress::from(address).floor();
        let (lifetime, page_count) = self
            .areas
            .iter_from(&start)
            .find_map(|(_, area)| {
                let shared = area.shared_anonymous.as_ref()?;
                let membership = shared.sysv.as_ref()?;
                (area
                    .vpn_range
                    .start
                    .as_usize()
                    .checked_sub(start.as_usize())
                    == Some(shared.page_offset))
                .then(|| (membership.lifetime.clone(), shared.backing.page_count))
            })
            .ok_or(MemoryError::InvalidRange)?;
        let end = start
            .as_usize()
            .checked_add(page_count)
            .ok_or(MemoryError::InvalidRange)?;
        let mut keys = Vec::new();
        for (key, area) in self.areas.iter_from(&start) {
            if area.vpn_range.end.as_usize() > end {
                break;
            }
            if let Some(shared) = &area.shared_anonymous
                && let Some(membership) = &shared.sysv
                && Arc::ptr_eq(&membership.lifetime, &lifetime)
                && key.as_usize().checked_sub(start.as_usize()) == Some(shared.page_offset)
            {
                keys.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
                keys.push(*key);
            }
        }
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(keys.len())
            .map_err(|_| MemoryError::OutOfMemory)?;
        for key in keys {
            retired.push(self.remove_area(&key).expect("preflighted shm fragment"));
        }
        Ok(PreparedAreaRetirement::batch(retired, &mut self.page_table))
    }
}
