//! Shared-file write revocation keeps VMA permissions, retires claims only after the CPU fence.
use super::*;

impl MemorySet {
    pub(crate) fn revoke_shared_file_writers(&mut self, id: SharedFileId) {
        let mut commit = TranslationCommit::new();
        let page_table = &mut self.page_table;
        self.areas.for_each_mut(|_, area| {
            let Some(shared) = &area.shared_file else {
                return;
            };
            if shared.mapping.id() != id {
                return;
            }
            let permission = shared.leaf_permission(area.map_permission);
            assert!(
                !permission.contains(MapPermission::W),
                "write revocation requires a frozen backing"
            );
            for (&vpn, _) in &shared.resident {
                if page_table.translate(vpn).is_none() {
                    continue;
                }
                if MapArea::has_leaf_permission(permission) {
                    page_table
                        .set_flags(vpn, permission.into(), &mut commit)
                        .expect("resident shared leaf disappeared under mm ownership");
                } else {
                    page_table
                        .unmap(vpn, &mut commit)
                        .expect("resident shared leaf disappeared");
                }
            }
        });
        commit
            .synchronize()
            .expect("all-CPU shared write revocation fence failed");
        self.areas.for_each_mut(|_, area| {
            let Some(shared) = &mut area.shared_file else {
                return;
            };
            if shared.mapping.id() != id {
                return;
            }
            shared.resident.for_each_mut(|_, resident| {
                if resident.writer {
                    resident.page.release_writer();
                    resident.writer = false;
                }
            });
        });
    }
    /// 在 permission restriction 的 remote fence 完成后释放 shared-file writer claim。
    ///
    /// PTE 已撤销但远端 hart 仍可能使用旧 writable translation；提前释放 claim 会让
    /// writeback 与该 stale writer 并发。权限增加在 PTE 发布前 acquire，权限收紧则由此
    /// post-fence pass 释放，因而 writer 生命周期始终覆盖所有可写 translation。
    pub(super) fn release_restricted_shared_writers(
        &mut self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) {
        let page_table = &self.page_table;
        self.areas.for_each_mut(|_, area| {
            if end <= area.vpn_range.start || area.vpn_range.end <= start {
                return;
            }
            let Some(shared) = &mut area.shared_file else {
                return;
            };
            shared.resident.for_each_mut(|vpn, resident| {
                // Backing freeze can change while mprotect owns this mm. Only the actually
                // committed leaf proves that the completed fence removed write permission.
                let writable = page_table
                    .translate(*vpn)
                    .is_some_and(|pte| pte.permissions().contains(PagePermissions::WRITE));
                if resident.writer && !writable {
                    resident.page.release_writer();
                    resident.writer = false;
                }
            });
        });
    }
}
