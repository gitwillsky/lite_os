//! Linux VMA 与原生页表观测的汇总；诊断不修改 VMA、附加计数或内容后备。
use super::*;

impl MemorySet {
    /// 页统计；一次实际用户页表快照按 VMA 归属汇总，不用 backing 提交量估算 RSS。
    ///
    /// # Errors
    ///
    /// 原生 VMAR 查询或输出缓冲分配失败；不能把未知驻留量伪装成零或整个区间。
    pub(crate) fn user_page_statistics(&self) -> Result<UserPageStatistics, MemoryError> {
        let ranges = self
            .root
            .resident_ranges()
            .map_err(MemoryError::from_status)?;
        let mut cursor = 0;
        let mut statistics = UserPageStatistics::default();
        for (_, area) in self.areas.iter() {
            let pages = area.len() / PAGE_SIZE;
            statistics.virtual_pages += pages;
            while cursor < ranges.len()
                && ranges[cursor].base + ranges[cursor].size <= area.start as u64
            {
                cursor += 1;
            }
            let shared_anonymous = matches!(
                area.backing,
                Backing::SharedAnonymous { .. } | Backing::SystemV { .. }
            );
            for range in &ranges[cursor..] {
                if range.base >= area.end as u64 {
                    break;
                }
                let start = range.base.max(area.start as u64);
                let end = (range.base + range.size).min(area.end as u64);
                let pages = (end - start) as usize / PAGE_SIZE;
                statistics.resident_pages += pages;
                // Linux statm 的 shared 是文件/共享内存 RSS，不是 MAP_SHARED 或物理 mapcount。
                if shared_anonymous
                    || range.flags & native_runtime::native_abi::residency::ANONYMOUS == 0
                {
                    statistics.shared_pages += pages;
                }
            }
            if area.permission.contains(MapPermission::X) {
                statistics.text_pages += pages;
            }
            if area.counts_as_data() {
                statistics.data_pages += pages;
            }
            if matches!(
                area.backing,
                Backing::Anonymous {
                    role: AnonymousRole::Stack,
                    ..
                }
            ) {
                statistics.stack_pages += pages;
            }
        }
        Ok(statistics)
    }

    /// 把本地址空间的映射并入系统级分类；`seen` 是已计入后备对象的有序地址表。
    ///
    /// # Errors
    ///
    /// 去重表扩容失败。
    pub(crate) fn classify_mapped_memory(
        &self,
        seen: &mut Vec<usize>,
        totals: &mut MappedMemory,
    ) -> Result<(), MemoryError> {
        for (_, area) in self.areas.iter() {
            let object = area.backing.object();
            let key = Arc::as_ptr(object) as usize;
            let Err(position) = seen.binary_search(&key) else {
                continue;
            };
            seen.try_reserve(1).map_err(|_| MemoryError::OutOfMemory)?;
            seen.insert(position, key);
            let pages = object.committed_bytes()? as usize / PAGE_SIZE;
            match area.backing {
                Backing::Anonymous { .. } | Backing::PrivateFile { .. } => {
                    totals.anonymous_pages += pages;
                }
                Backing::SharedAnonymous { .. } | Backing::SystemV { .. } => {
                    totals.mapped_pages += pages;
                    totals.shmem_pages += pages;
                }
                Backing::SharedFile { .. } => totals.mapped_pages += pages,
                // 设备窗口不是可回收内存，trampoline 是内部页；Linux 同样不计入这些映射。
                Backing::Device { .. } | Backing::Trampoline { .. } => {}
            }
        }
        Ok(())
    }

    /// 复制本次 VMA 视图；调用者持 MM 锁，返回后不再借用索引，可在锁外查询文件身份。
    ///
    /// # Errors
    /// 快照容量分配失败返回 OutOfMemory，不发布不完整列表。
    pub(crate) fn mapping_snapshot(
        &self,
        detailed: bool,
    ) -> Result<Vec<MappingSnapshot>, MemoryError> {
        let mut areas = Vec::new();
        areas
            .try_reserve_exact(self.areas.len())
            .map_err(|_| MemoryError::OutOfMemory)?;
        let resident = if detailed {
            self.root
                .mapping_statistics()
                .map_err(MemoryError::from_status)?
        } else {
            Vec::new()
        };
        let mut cursor = 0;
        for area in self.areas.values() {
            let mut snapshot = area.observe();
            if detailed {
                while cursor < resident.len()
                    && resident[cursor].base + resident[cursor].size <= area.start as u64
                {
                    cursor += 1;
                }
                let shared_anonymous = matches!(
                    area.backing,
                    Backing::SharedAnonymous { .. } | Backing::SystemV { .. }
                );
                for range in &resident[cursor..] {
                    if range.base >= area.end as u64 {
                        break;
                    }
                    let start = range.base.max(area.start as u64);
                    let end = (range.base + range.size).min(area.end as u64);
                    snapshot
                        .statistics
                        .account(range, end - start, shared_anonymous);
                }
            }
            areas.push(snapshot);
        }
        Ok(areas)
    }
}
