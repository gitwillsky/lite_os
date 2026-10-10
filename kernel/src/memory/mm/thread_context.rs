//! Thread supervisor context 的实际 VMA 分配与回收；编号不参与地址计算。
use super::*;

impl MemorySet {
    /// 在架构声明的 context 区间选择实际空闲页并原子映射。
    /// 普通追加/刚退休页重用只查一个 ordered neighbor；冲突跳过整段 VMA，耗尽后绕回一次。
    /// 无可用 placement/页位或映射分配失败返回 MemoryError，失败不改变 hint/VMA/PTE。
    pub(crate) fn allocate_thread_trap_context(&mut self) -> Result<usize, MemoryError> {
        let range = crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE.ok_or(MemoryError::InvalidRange)?;
        let mut top = self
            .thread_context_hint
            .unwrap_or(range.end)
            .clamp(range.start, range.end);
        for _ in 0..2 {
            while let Some(address) = top
                .checked_sub(config::PAGE_SIZE)
                .filter(|address| *address >= range.start)
            {
                let vpn = VirtualAddress::from(address).floor();
                if let Some((_, area)) = self.areas.floor(&vpn)
                    && vpn < area.vpn_range.end
                {
                    top = usize::from(VirtualAddress::from(area.vpn_range.start));
                    continue;
                }
                self.insert_framed_area(
                    address.into(),
                    top.into(),
                    MapPermission::R | MapPermission::W,
                )?;
                self.thread_context_hint = Some(address);
                return Ok(address);
            }
            top = range.end;
        }
        Err(MemoryError::OutOfMemory)
    }

    /// 退休已退出 Thread 的唯一 temporary context 页；调用者已经撤销 context owner。
    /// 实际 MM guard/fence 序列保持映射 ownership，缺失映射表示重复清理并 fail-stop。
    pub(crate) fn remove_thread_trap_context(&mut self, address: usize) {
        let range = crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE
            .expect("temporary context requires address-space backing");
        assert!(range.contains(&address) && address.is_multiple_of(config::PAGE_SIZE));
        let vpn = VirtualAddress::from(address).floor();
        self.remove_area_with_start_vpn(vpn);
        self.thread_context_hint = Some(address + config::PAGE_SIZE);
    }
}
