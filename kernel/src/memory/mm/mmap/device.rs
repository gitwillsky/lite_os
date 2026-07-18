use super::*;

impl MemorySet {
    /// @description 建立直接映射 DRM/GEM physical extent 的共享 device VMA。
    ///
    /// @param address 零表示由内核选址；非零是 hint 或 exact 地址。
    /// @param length 非零字节长度，不得超过 backing extent。
    /// @param permission 用户 read/write/none 权限；device mapping 不可执行。
    /// @param fixed_noreplace 为真时必须精确使用 address，冲突不替换。
    /// @param source DRM 已完成 handle/offset/length 授权的 backing view。
    /// @param address_space_limit 当前 Process `RLIMIT_AS` soft limit。
    /// @return 成功返回映射起始地址；失败不留下 PTE 或 VMA owner。
    pub(crate) fn map_device(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        source: DeviceMappingSource,
        address_space_limit: u64,
    ) -> Result<usize, MemoryError> {
        if length == 0
            || !permission.contains(MapPermission::U)
            || permission.contains(MapPermission::X)
            || (fixed_noreplace && (address == 0 || !VirtualAddress::from(address).is_aligned()))
        {
            return Err(MemoryError::InvalidRange);
        }
        let page_count = length
            .checked_add(config::PAGE_SIZE - 1)
            .ok_or(MemoryError::InvalidRange)?
            / config::PAGE_SIZE;
        if source
            .page_offset
            .checked_add(page_count)
            .is_none_or(|end| end > source.backing.pages())
        {
            return Err(MemoryError::InvalidRange);
        }
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
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::device(start.into(), end.into(), permission, source),
            None,
        )?;
        Self::flush_tlb_all_cpus()
            .expect("platform TLB synchronization failed after device mmap update");
        Ok(start)
    }
}
