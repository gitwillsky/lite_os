//! 原生映射观测：区分已有 backing 与实际用户翻译，不触发 fault 或 pager。
use super::*;

impl Mapping {
    pub(in crate::vm) fn vmo(&self) -> &Arc<Vmo> {
        &self.vmo
    }

    pub(in crate::vm) fn object_page(&self, page: usize) -> usize {
        self.vmo_page + page - self.base / PAGE_SIZE
    }

    /// 调用者持 VMO 树锁保活页框；每个逻辑映射只观察一次，同一服务区的硬件镜像去重。
    pub(in crate::vm) fn resident_pages(&self) -> Result<Vec<(usize, usize)>, VmError> {
        let mut pages = Vec::new();
        let Some(owner) = self.space.upgrade() else {
            return Ok(pages);
        };
        let inner = owner.lock()?;
        if !self.live.load(Ordering::Acquire) {
            return Ok(pages);
        }
        let table = inner.views.resident_table();
        let mut cursor = self.base / PAGE_SIZE;
        let end = self.end() / PAGE_SIZE;
        while let Some((page, entry)) = table.next_mapped(cursor, end) {
            cursor = page + 1;
            if entry.permissions().contains(PagePermissions::USER) {
                pages.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                pages.push((page, entry.ppn().as_usize()));
            }
        }
        Ok(pages)
    }
}

impl Vmar {
    /// 映射归属按树分组，每棵树只构造一次临时共享计数；不在持空间锁时等待 VMO 树锁。
    pub(crate) fn mapping_statistics(&self) -> Result<Vec<native_abi::VmarMappingStats>, VmError> {
        self.observed_mappings().map(|(_, ranges)| ranges)
    }

    /// 本 VMAR 的逻辑驻留汇总；成员只拥有私有区，共享服务区由实际 owner 计费一次。
    pub(crate) fn memory_stats(&self) -> Result<native_abi::TaskStats, VmError> {
        let (mapped_bytes, ranges) = self.observed_mappings()?;
        let mut stats = native_abi::TaskStats {
            mapped_bytes,
            ..Default::default()
        };
        let mut scaled_shared = 0;
        for range in ranges {
            stats.resident_bytes += range.size;
            if range.flags & native_abi::residency::SHARED == 0 {
                stats.private_bytes += range.size;
            } else {
                stats.shared_bytes += range.size;
                scaled_shared += range.pss_per_page * (range.size / PAGE_SIZE as u64);
            }
        }
        stats.scaled_shared_bytes = scaled_shared >> 12;
        Ok(stats)
    }

    fn observed_mappings(&self) -> Result<(u64, Vec<native_abi::VmarMappingStats>), VmError> {
        let mut mappings = self.mapping_snapshot()?;
        let mapped_bytes = mappings.iter().map(|mapping| mapping.size as u64).sum();
        mappings.sort_unstable_by_key(|mapping| (mapping.vmo.tree_identity(), mapping.base));
        let mut output = Vec::new();
        let mut start = 0;
        while start < mappings.len() {
            let first = &mappings[start];
            let tree = first.vmo.tree_identity();
            let end = start
                + mappings[start..].partition_point(|mapping| mapping.vmo.tree_identity() == tree);
            first
                .vmo
                .mapping_statistics(&mappings[start..end], &mut output)?;
            start = end;
        }
        output.sort_unstable_by_key(|mapping| mapping.base);
        Ok((mapped_bytes, output))
    }

    fn mapping_snapshot(&self) -> Result<Vec<Arc<Mapping>>, VmError> {
        let mappings = {
            let inner = self.space.inner.lock().map_err(|_| VmError::NoMemory)?;
            if !inner.nodes.contains_key(&self.id) {
                return Err(VmError::BadState);
            }
            let mut mappings = Vec::new();
            for node in inner.nodes.values() {
                for child in node.children.values() {
                    if let Child::Mapping(mapping) = child
                        && mapping.base >= self.base
                        && mapping.end() <= self.base + self.size
                    {
                        mappings.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                        mappings.push(mapping.clone());
                    }
                }
            }
            mappings
        };
        Ok(mappings)
    }

    /// 只观察 RAM 翻译与内容来源；不扫描共享映射，成员不计共享服务区或 supervisor 页。
    pub(crate) fn resident_ranges(&self) -> Result<Vec<native_abi::VmarResidentRange>, VmError> {
        let mut mappings = self.mapping_snapshot()?;
        mappings.sort_unstable_by_key(|mapping| mapping.base);
        let mut ranges = Vec::new();
        for mapping in mappings {
            mapping.vmo.resident_ranges(&mapping, &mut ranges)?;
        }
        Ok(ranges)
    }

    /// 区间信息。
    pub(crate) fn info(&self) -> VmarInfo {
        VmarInfo {
            base: self.base,
            size: self.size,
        }
    }
}
