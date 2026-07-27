use super::*;

impl MapArea {
    /// @description 撤销当前 VMA 的 leaf/reserved translation，但继续保留全部 backing owner。
    ///
    /// @param page_table 当前 AddressSpace 的唯一页表 owner。
    /// @return 无返回值；不存在的 leaf 表示尚未 fault-in 或 rollback 已撤销。
    /// @note live mapping 的 caller 必须在同步 TLB fence 完成后才 Drop self；缺少该顺序会让
    /// remote CPU 通过 stale translation 访问已经复用的 frame 或已释放的 device backing。
    pub(in crate::memory) fn unmap(
        &mut self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) {
        if self.device.is_some() {
            self.unmap_device_area(page_table, commit);
            return;
        }
        if self.lazy_private || self.shared_anonymous.is_some() || self.shared_file.is_some() {
            for (&vpn, _) in &self.data_frames {
                let _ = page_table.unmap(vpn, commit);
            }
            if let Some(shared) = &self.shared_file {
                for (&vpn, _) in &shared.resident {
                    let _ = page_table.unmap(vpn, commit);
                }
            }
        } else {
            for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
                let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn), commit);
            }
        }
    }
}

/// 已从唯一 MemorySet owner 摘除、但 backing 仍等待 remote TLB completion 的 VMA。
pub(crate) struct PreparedAreaRetirement {
    area: Option<MapArea>,
    commit: TranslationCommit,
}

impl PreparedAreaRetirement {
    /// @description 同步完成 detached VMA 的 translation fence，再释放全部 backing owner。
    ///
    /// @return 无返回值。
    /// @errors platform shootdown 失败时保留 owner 并 fail-stop。
    pub(crate) fn synchronize(mut self) {
        self.commit
            .synchronize()
            .expect("platform TLB synchronization failed while retiring VMA");
        let mut area = self
            .area
            .take()
            .expect("detached VMA retirement completed twice");
        area.data_frames.clear();
        if let Some(shared) = &mut area.shared_file {
            shared.resident.clear();
        }
    }
}

impl Drop for PreparedAreaRetirement {
    fn drop(&mut self) {
        let Some(area) = self.area.take() else {
            return;
        };
        // 未完成 fence 时必须泄漏 backing 与 detached table owners；正常路径由 synchronize
        // 先清空 area。若直接 Drop，释放 frame 会让 stale translation 访问复用内存。
        core::mem::forget(area);
        let commit = core::mem::replace(&mut self.commit, TranslationCommit::new());
        core::mem::forget(commit);
        debug_assert!(false, "detached VMA retired without TLB synchronization");
    }
}

impl MemorySet {
    /// @description 在页表 owner 内摘除 VMA/PTE，但把 backing 与 fence token 移交给 caller。
    ///
    /// @param start_vpn 待移除 area 的精确起始 VPN。
    /// @return 可在 MemorySet lock 释放后同步的唯一 retirement owner。
    /// @errors 目标 VMA 缺失时 fail-stop。
    pub(crate) fn prepare_area_retirement(
        &mut self,
        start_vpn: VirtualPageNumber,
    ) -> PreparedAreaRetirement {
        let mut area = self
            .remove_area(&start_vpn)
            .expect("retired VMA start must remain present");
        let mut commit = TranslationCommit::new();
        area.unmap(&mut self.page_table, &mut commit);
        PreparedAreaRetirement {
            area: Some(area),
            commit,
        }
    }

    /// @description 撤销并释放一个完整 area，同时封闭 remote stale translation 窗口。
    ///
    /// @param start_vpn 待移除 area 的精确起始 VPN。
    /// @return fence 完成后释放 area owner；目标缺失或 shootdown 失败时 fail-stop。
    pub(crate) fn remove_area_with_start_vpn(&mut self, start_vpn: VirtualPageNumber) {
        self.prepare_area_retirement(start_vpn).synchronize();
    }
}
