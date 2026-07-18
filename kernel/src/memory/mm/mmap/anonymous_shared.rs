use super::*;

impl MemorySet {
    /// @description 建立 eager anonymous shared mapping；backing 是所有 fork descendant 页帧
    /// 与 futex identity 的唯一 owner。
    ///
    /// @param address 零表示由内核选址；非零是 hint 或 exact 地址。
    /// @param length 非零字节长度，向上取整到整页。
    /// @param permission 用户页权限；允许 PROT_NONE 与 Linux W+X 映射。
    /// @param fixed_noreplace 为真时必须精确使用 address，冲突不替换。
    /// @return 成功返回映射起始地址；分配或页表提交失败不留下 VMA。
    pub(crate) fn map_shared_anonymous(
        &mut self,
        address: usize,
        length: usize,
        permission: MapPermission,
        fixed_noreplace: bool,
        address_space_limit: u64,
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
        let backing = AnonymousSharedBacking::allocate(page_count)?;
        let start = usize::from(VirtualAddress::from(range.start));
        let end = usize::from(VirtualAddress::from(range.end));
        self.push(
            MapArea::shared_anonymous(start.into(), end.into(), permission, backing),
            None,
        )?;
        Self::flush_tlb_all_cpus()
            .expect("platform TLB synchronization failed after anonymous shared mmap update");
        Ok(start)
    }
}
