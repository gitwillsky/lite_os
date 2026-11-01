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

    /// 统计 `range` 与现有 VMA 相交部分对 RLIMIT_AS/RLIMIT_DATA 的贡献，即替换它们会释放的 usage。
    ///
    /// 从 ordered floor 开始，只遍历实际相交节点；不扫描不相关 VMA。
    ///
    /// # Returns
    ///
    /// `(virtual_bytes, data_bytes)`。
    pub(super) fn replaced_usage(&self, range: &Range<VirtualPageNumber>) -> (u64, u64) {
        let first = self
            .areas
            .floor(&range.start)
            .map_or(range.start, |(key, _)| *key);
        self.areas
            .iter_from(&first)
            .take_while(|(key, _)| **key < range.end)
            .filter(|(_, area)| range.start < area.vpn_range.end)
            .fold((0u64, 0u64), |(virtual_bytes, data_bytes), (_, area)| {
                let overlap = (range.end.min(area.vpn_range.end).as_usize()
                    - range.start.max(area.vpn_range.start).as_usize())
                    as u64
                    * config::PAGE_SIZE as u64;
                let contribution = area.index_contribution();
                (
                    virtual_bytes
                        + if contribution.virtual_bytes != 0 {
                            overlap
                        } else {
                            0
                        },
                    data_bytes
                        + if contribution.data_bytes != 0 {
                            overlap
                        } else {
                            0
                        },
                )
            })
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
