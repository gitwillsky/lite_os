//! 按实际物理页映射次数分摊驻留；查询期间的临时索引，不增加页热路径或持久 mapcount 状态。
use super::*;

struct ResidentContent {
    anonymous: bool,
    dirty: bool,
    volatile_pager: bool,
}

impl Vmo {
    /// 便宜的 RSS/content 观测；不构造同树共享计数，也不把物理设备映射当成 RAM。
    pub(in crate::vm) fn resident_ranges(
        self: &Arc<Self>,
        mapping: &Mapping,
        output: &mut Vec<native_abi::VmarResidentRange>,
    ) -> Result<(), VmError> {
        let _tree = self.lock_tree()?;
        for (page, frame) in mapping.resident_pages()? {
            let Some(content) = self.resident_content(mapping.object_page(page), frame) else {
                continue;
            };
            let mut flags = 0;
            if content.anonymous {
                flags |= native_abi::residency::ANONYMOUS;
            }
            if content.volatile_pager {
                flags |= native_abi::residency::VOLATILE_PAGER;
            }
            let base = (page * PAGE_SIZE) as u64;
            if let Some(last) = output.last_mut()
                && last.base + last.size == base
                && last.flags == flags
            {
                last.size += PAGE_SIZE as u64;
            } else {
                output.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                output.push(native_abi::VmarResidentRange {
                    base,
                    size: PAGE_SIZE as u64,
                    flags,
                });
            }
        }
        Ok(())
    }

    /// 仅作同一次观测内的分组键；调用者保活 VMO，地址不会在分组期间复用。
    pub(in crate::vm) fn tree_identity(&self) -> usize {
        Arc::as_ptr(&self.tree) as usize
    }

    /// 一组同树映射的实际驻留归属；树锁保护物理页寿命，地址空间锁只在读页表时持有。
    pub(in crate::vm) fn mapping_statistics(
        self: &Arc<Self>,
        targets: &[Arc<Mapping>],
        output: &mut Vec<native_abi::VmarMappingStats>,
    ) -> Result<(), VmError> {
        let _tree = self.lock_tree()?;
        // 1. 从唯一树 owner 找到全部逻辑映射；Weak 不增加另一份注册或页框 owner。
        let mut root = self.clone();
        loop {
            let parent = match &root.state.lock().relation {
                Some(Relation::Cow { parent, .. } | Relation::Slice { parent, .. }) => {
                    Some(parent.clone())
                }
                None => None,
            };
            let Some(parent) = parent else { break };
            root = parent;
        }
        let mut pending = Vec::new();
        pending.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        pending.push(root);
        let mut frames = Vec::<usize>::new();
        while let Some(node) = pending.pop() {
            let mappings = {
                let state = node.state.lock();
                pending
                    .try_reserve(state.children.len())
                    .map_err(|_| VmError::NoMemory)?;
                pending.extend(state.children.iter().filter_map(Weak::upgrade));
                let mut mappings = Vec::new();
                mappings
                    .try_reserve_exact(state.mappings.len())
                    .map_err(|_| VmError::NoMemory)?;
                mappings.extend(state.mappings.iter().filter_map(Weak::upgrade));
                mappings
            };
            for mapping in mappings {
                for (page, frame) in mapping.resident_pages()? {
                    if node
                        .resident_content(mapping.object_page(page), frame)
                        .is_some()
                    {
                        frames.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                        frames.push(frame);
                    }
                }
            }
        }
        // 2. 相同物理页相邻，两个二分边界给出映射次数；虚拟别名分别计数，硬件镜像已去重。
        frames.sort_unstable();
        for mapping in targets {
            for (page, frame) in mapping.resident_pages()? {
                let Some(content) = mapping
                    .vmo()
                    .resident_content(mapping.object_page(page), frame)
                else {
                    continue;
                };
                let copies = frames.partition_point(|other| *other <= frame)
                    - frames.partition_point(|other| *other < frame);
                // 查询不冻结其他空间的 munmap/publication；同树页框仍保活，但映射次数是瞬时观测。
                let mut flags = 0;
                if copies > 1 {
                    flags |= native_abi::residency::SHARED;
                }
                if content.anonymous {
                    flags |= native_abi::residency::ANONYMOUS;
                }
                if content.dirty {
                    flags |= native_abi::residency::DIRTY;
                }
                if content.volatile_pager {
                    flags |= native_abi::residency::VOLATILE_PAGER;
                }
                // 先按页分摊并保留小数；区间可在任意页边界拆分，不把平均量摊给未驻留页。
                let stats = native_abi::VmarMappingStats {
                    base: (page * PAGE_SIZE) as u64,
                    size: PAGE_SIZE as u64,
                    pss_per_page: ((PAGE_SIZE as u64) << 12) / copies.max(1) as u64,
                    flags,
                };
                if let Some(last) = output.last_mut()
                    && last.base + last.size == stats.base
                    && last.flags == stats.flags
                    && last.pss_per_page == stats.pss_per_page
                {
                    last.size += PAGE_SIZE as u64;
                } else {
                    output.try_reserve(1).map_err(|_| VmError::NoMemory)?;
                    output.push(stats);
                }
            }
        }
        Ok(())
    }

    /// 调用者已持树锁；只解析已有页的来源，不补页、不做 COW、不访问页内容。
    fn resident_content(
        self: &Arc<Self>,
        mut index: usize,
        frame: usize,
    ) -> Option<ResidentContent> {
        let mut node = self.clone();
        loop {
            let state = node.state.lock();
            if index >= state.pages {
                return None;
            }
            match &node.kind {
                Kind::Physical { .. } => return None,
                Kind::Contiguous(frames) => {
                    return (frames.ppn.as_usize() + index == frame).then_some(ResidentContent {
                        anonymous: false,
                        dirty: true,
                        volatile_pager: false,
                    });
                }
                _ => {}
            }
            if let Some(page) = state.committed.get(&index) {
                return (page.frame.ppn.as_usize() == frame).then_some(ResidentContent {
                    anonymous: page.content == PageContent::Anonymous,
                    dirty: page.content != PageContent::Bootfs
                        && page.cleanliness == Cleanliness::Dirty,
                    volatile_pager: page.content == PageContent::AnonymousPager,
                });
            }
            let (parent, offset) = match &state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) if index < *limit => (parent.clone(), *offset),
                Some(Relation::Slice { parent, offset }) => (parent.clone(), *offset),
                _ => return None,
            };
            drop(state);
            index += offset;
            node = parent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_content_keeps_sparse_cow_and_slice_sources_without_faulting() {
        assert_eq!(core::mem::size_of::<Page>(), 24);
        let parent = Vmo::create((PAGE_SIZE * 8) as u64, VmoOptions::default()).unwrap();
        parent.write(PAGE_SIZE as u64, &[17]).unwrap();
        parent.write((PAGE_SIZE * 6) as u64, &[23]).unwrap();
        let child = parent
            .create_child(0, parent.size(), ChildKind::Snapshot)
            .unwrap();
        {
            let _tree = child.lock_tree().unwrap();
            let frame = child.resolve_locked(1, false).ok().unwrap().ppn.as_usize();
            assert!(child.resident_content(1, frame).is_some());
            assert!(child.resident_content(2, frame).is_none());
        }
        assert_eq!(child.committed_bytes().unwrap(), 0);
        child.write(PAGE_SIZE as u64, &[31]).unwrap();
        let slice = child
            .create_child(PAGE_SIZE as u64, (PAGE_SIZE * 6) as u64, ChildKind::Slice)
            .unwrap();
        {
            let _tree = child.lock_tree().unwrap();
            let frame = child.resolve_locked(1, false).ok().unwrap().ppn.as_usize();
            assert!(child.resident_content(1, frame).is_some());
            assert!(slice.resident_content(0, frame).is_some());
            assert!(child.resident_content(6, frame).is_none());
        }
        assert_eq!(child.committed_bytes().unwrap(), PAGE_SIZE as u64);
        let mut value = [0];
        parent.read(PAGE_SIZE as u64, &mut value).unwrap();
        assert_eq!(value[0], 17);
    }
}
