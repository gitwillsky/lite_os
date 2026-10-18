use super::*;

impl MapArea {
    /// 撤销当前 VMA 的 leaf/reserved translation，但继续保留全部 backing owner。
    ///
    /// live mapping 的 caller 必须在同步 TLB fence 完成后才 Drop self；缺少该顺序会让
    /// remote CPU 通过 stale translation 访问已经复用的 frame 或已释放的 device backing。
    ///
    /// # Parameters
    ///
    /// - `page_table`: 当前 AddressSpace 的唯一页表 owner。
    ///
    /// # Returns
    ///
    /// 无返回值；不存在的 leaf 表示尚未 fault-in 或 rollback 已撤销。
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

/// 已从唯一 MemorySet 摘除、等待 remote TLB completion 的 backing 集合。
/// single 不分配；batch 使用 munmap 在修改页表前预留的容量。
enum RetiredAreas {
    Single(MapArea),
    Batch(Vec<MapArea>),
}

/// VMA backing 与 translation fence 的唯一 retirement owner。
/// caller 必须先释放 MemorySet lock，再同步并析构文件与设备资源。
pub(crate) struct PreparedAreaRetirement {
    areas: Option<RetiredAreas>,
    commit: TranslationCommit,
}

impl PreparedAreaRetirement {
    pub(super) fn batch(mut areas: Vec<MapArea>, page_table: &mut PageTable) -> Self {
        let mut commit = TranslationCommit::new();
        for area in &mut areas {
            area.unmap(page_table, &mut commit);
        }
        Self {
            areas: Some(RetiredAreas::Batch(areas)),
            commit,
        }
    }

    /// 构造 remap 的 source fence：`areas` 是被丢弃的尾部，`commit` 已包含搬迁页的旧翻译撤销。
    ///
    /// 被搬迁的 VMA 不在 `areas` 里，它已经带着全部 owner 重新发布到新地址。
    pub(super) fn rehoming(areas: Vec<MapArea>, commit: TranslationCommit) -> Self {
        Self {
            areas: Some(RetiredAreas::Batch(areas)),
            commit,
        }
    }

    /// 在 MM lock 外完成 fence，随后释放实际 file/backend/frame owner。
    ///
    /// # Errors
    ///
    /// platform shootdown 失败时保留 backing 与 detached table owners 并 fail-stop。
    pub(crate) fn synchronize(mut self) {
        if self.commit.synchronize().is_err() {
            self.preserve_unfenced();
            panic!("platform TLB synchronization failed while retiring VMA");
        }
        let areas = self.areas.take().expect("VMA retirement completed twice");
        match areas {
            RetiredAreas::Single(area) => Self::release(area),
            RetiredAreas::Batch(areas) => {
                for area in areas {
                    Self::release(area);
                }
            }
        }
    }

    fn release(mut area: MapArea) {
        // shared_file 的 mapping 字段先于 resident；必须先归还页写者，再运行最后 vm_file Drop。
        // 否则 close/shutdown 可以在页仍持 writer claim 时冻结或退休 backing。
        area.data_frames.clear();
        if let Some(shared) = &mut area.shared_file {
            shared.resident.clear();
        }
        drop(area);
    }

    fn preserve_unfenced(&mut self) -> bool {
        let Some(areas) = self.areas.take() else {
            return false;
        };
        core::mem::forget(areas);
        let commit = core::mem::replace(&mut self.commit, TranslationCommit::new());
        core::mem::forget(commit);
        true
    }
}

impl Drop for PreparedAreaRetirement {
    fn drop(&mut self) {
        // 未完成 fence 的 token 不能释放 backing：stale translation 仍可能访问这些页。
        // synchronize 成功取出 areas；遗漏调用与 fence 失败都保留完整资源并 fail-stop。
        if self.preserve_unfenced() {
            panic!("detached VMA retired without TLB synchronization");
        }
    }
}

impl MemorySet {
    /// 在页表 owner 内摘除 VMA/PTE，但把 backing 与 fence token 移交给 caller。
    ///
    /// # Parameters
    ///
    /// - `start_vpn`: 待移除 area 的精确起始 VPN。
    ///
    /// # Returns
    ///
    /// 可在 MemorySet lock 释放后同步的唯一 retirement owner。
    ///
    /// # Errors
    ///
    /// 目标 VMA 缺失时 fail-stop。
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
            areas: Some(RetiredAreas::Single(area)),
            commit,
        }
    }

    /// 撤销并释放一个完整 area，同时封闭 remote stale translation 窗口。
    ///
    /// # Parameters
    ///
    /// - `start_vpn`: 待移除 area 的精确起始 VPN。
    ///
    /// # Returns
    ///
    /// fence 完成后释放 area owner；目标缺失或 shootdown 失败时 fail-stop。
    pub(crate) fn remove_area_with_start_vpn(&mut self, start_vpn: VirtualPageNumber) {
        self.prepare_area_retirement(start_vpn).synchronize();
    }
}
