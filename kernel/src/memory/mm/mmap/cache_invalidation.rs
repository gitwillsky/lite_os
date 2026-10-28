//! 直接 I/O 只撤销文件 cache aliases，不能像 truncate 一样丢弃 private COW 数据。
use super::*;

impl MemorySet {
    /// 撤销 `id` 在闭区间 `first..=last` 的缓存映射；fence 后释放 resident，不分配且不改变 VMA。
    pub(crate) fn invalidate_cached_file(&mut self, id: SharedFileId, first: u64, last: u64) {
        // 1. 只撤销仍指向 cache 的 PTE；没有映射的 PROT_NONE resident 也在后续释放。
        let memory = revoke_and_commit(self, |memory, commit| {
            let page_table = &mut memory.page_table;
            memory.areas.for_each_mut(|_, area| {
                if let Some(shared) = &area.shared_file {
                    if shared.mapping.id() == id {
                        for (&vpn, _) in &shared.resident {
                            let relative =
                                (vpn.as_usize() - area.vpn_range.start.as_usize()) as u64;
                            if shared
                                .pages
                                .page(relative)
                                .is_some_and(|page| first <= page && page <= last)
                            {
                                let _ = page_table.unmap(vpn, commit);
                            }
                        }
                    }
                } else if let Some(private) = &area.private_file {
                    for (&vpn, resident) in &area.data_frames {
                        if matches!(resident.backing, PrivateBacking::File { .. })
                            && private.cached_identity(vpn).is_some_and(|(file, page)| {
                                file == id && first <= page && page <= last
                            })
                        {
                            let _ = page_table.unmap(vpn, commit);
                        }
                    }
                }
            });
        })
        .expect("platform TLB synchronization failed after cache invalidation");
        // 2. 远端 fence 完成后释放同一 resident；private COW 已独立于文件，必须继续保留。
        memory.areas.for_each_mut(|_, area| {
            if let Some(shared) = &mut area.shared_file {
                if shared.mapping.id() == id {
                    let start = area.vpn_range.start.as_usize();
                    shared.resident.retain(|vpn, _| {
                        let relative = (vpn.as_usize() - start) as u64;
                        !shared
                            .pages
                            .page(relative)
                            .is_some_and(|page| first <= page && page <= last)
                    });
                }
            } else if let Some(private) = &area.private_file {
                area.data_frames.retain(|vpn, resident| {
                    !matches!(resident.backing, PrivateBacking::File { .. })
                        || !private
                            .cached_identity(*vpn)
                            .is_some_and(|(file, page)| file == id && first <= page && page <= last)
                });
            }
        });
    }
}
