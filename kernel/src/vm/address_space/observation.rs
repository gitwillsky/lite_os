//! 原生映射驻留统计；只观察 backing，不触发 fault 或 pager。
use super::*;

impl AddressSpace {
    /// 映射快照在地址空间锁外查询 VMO，保持 VMO tree → address-space 的既有锁序。
    /// 查询不安装翻译；共享服务区在本次进程快照中包含一次，RSS 不声明物理页去重。
    pub(crate) fn memory_stats(&self) -> Result<(u64, u64), VmError> {
        let mappings = {
            let inner = self.inner.lock().map_err(|_| VmError::NoMemory)?;
            let mut mappings = Vec::new();
            for node in inner.nodes.values() {
                for child in node.children.values() {
                    if let Child::Mapping(mapping) = child {
                        mappings.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                        mappings.push(mapping.clone());
                    }
                }
            }
            mappings
        };
        let (mut mapped, mut resident) = (0u64, 0u64);
        for mapping in mappings {
            mapped = mapped.saturating_add(mapping.size as u64);
            resident = resident.saturating_add(
                (mapping
                    .vmo
                    .resident_pages(mapping.vmo_page, mapping.size / PAGE_SIZE)?
                    * PAGE_SIZE) as u64,
            );
        }
        if let Some(shared) = &self.shared {
            let (shared_mapped, shared_resident) = shared.memory_stats()?;
            mapped = mapped.saturating_add(shared_mapped);
            resident = resident.saturating_add(shared_resident);
        }
        Ok((mapped, resident))
    }
}

impl Vmar {
    /// 区间信息。
    pub(crate) fn info(&self) -> VmarInfo {
        VmarInfo {
            base: self.base,
            size: self.size,
        }
    }
}
