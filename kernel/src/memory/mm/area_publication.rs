//! VMA/PTE 的 fallible publication 与唯一有序 index accounting seam；仍由 MemorySet 同锁拥有。
use super::*;

impl MemorySet {
    /// 使用 ordered neighbors 判断 `[start,end)` 是否与任意 live VMA 相交。
    ///
    /// # Parameters
    ///
    /// - `start`: inclusive 起始 VPN。
    /// - `end`: exclusive 结束 VPN。
    ///
    /// # Returns
    ///
    /// 区间非空且 floor/ceiling 均不相交时为 true。
    pub(super) fn range_is_free(&self, start: VirtualPageNumber, end: VirtualPageNumber) -> bool {
        if start >= end
            || self
                .areas
                .floor(&start)
                .is_some_and(|(_, area)| start < area.vpn_range.end)
        {
            return false;
        }
        self.areas
            .ceiling(&start)
            .is_none_or(|(key, _)| *key >= end)
    }

    pub(crate) fn push(
        &mut self,
        map_area: MapArea,
        data: Option<&[u8]>,
    ) -> Result<(), MemoryError> {
        let start = map_area.vpn_range.start;
        let end = map_area.vpn_range.end;
        if !self.range_is_free(start, end) {
            return Err(MemoryError::AddressInUse);
        }
        // VMA node 必须先于 PTE publication 分配；缺少该 preflight 会在页表已经
        // 可见后因 ordered-index node OOM 无法记录其唯一 owner。
        let mut prepared = self
            .areas
            .try_prepare_vacant(start, map_area)
            .map_err(|_| MemoryError::OutOfMemory)?;
        let mut publication = TranslationCommit::new();
        if let Err(e) = prepared
            .value_mut()
            .map(&mut self.page_table, &mut publication)
        {
            // VMA node 尚未发布，但 live shared mm 的 hardware walker 已可观察 partial PTE；
            // rollback 仍须保留 prepared area owner 直到所有 CPU 完成 fence。
            let prepared = revoke_and_commit(prepared, |prepared, rollback| {
                prepared.value_mut().unmap(&mut self.page_table, rollback);
            })
            .expect("platform TLB synchronization failed during VMA map rollback");
            drop(prepared);
            return Err(e);
        }
        if let Some(data) = data
            && let Err(error) = prepared.value_mut().copy_data(data)
        {
            let prepared = revoke_and_commit(prepared, |prepared, rollback| {
                prepared.value_mut().unmap(&mut self.page_table, rollback);
            })
            .expect("platform TLB synchronization failed during VMA data rollback");
            drop(prepared);
            return Err(error);
        }
        publication
            .synchronize()
            .expect("local translation fence failed during VMA publication");
        self.commit_area(prepared);
        Ok(())
    }

    pub(crate) fn insert_framed_area(
        &mut self,
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        )
    }

    pub(super) fn commit_area(&mut self, entry: VacantEntry<VirtualPageNumber, MapArea>) {
        self.vma_index_state
            .publish(entry.value().index_contribution());
        self.areas.commit_vacant(entry);
    }
    pub(super) fn take_area_entry(
        &mut self,
        key: &VirtualPageNumber,
    ) -> Option<VacantEntry<VirtualPageNumber, MapArea>> {
        let entry = self.areas.take_entry(key)?;
        self.vma_index_state
            .retire(entry.value().index_contribution());
        Some(entry)
    }
    pub(super) fn remove_area(&mut self, key: &VirtualPageNumber) -> Option<MapArea> {
        self.take_area_entry(key).map(VacantEntry::into_value)
    }
}
