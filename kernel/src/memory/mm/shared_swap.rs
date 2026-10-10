//! Shared-file canonical pages 的 alias revoke 与 logical swapoff unuse。

use super::*;
use crate::memory::{PhysicalPageNumber, SwappablePage};

impl MemorySet {
    /// caller 已 try-MM 成功，且持 PageOp；不等待任何其他 MM 或重新取得 PageOp。
    pub(crate) fn revoke_swappable_page(
        &mut self,
        page: &SwappablePage,
        ppn: PhysicalPageNumber,
    ) -> usize {
        let mut count = 0usize;
        let mut commit = TranslationCommit::new();
        for area in self.areas.values() {
            for (&vpn, resident) in &area.data_frames {
                if resident
                    .mapping()
                    .is_some_and(|mapping| mapping.matches(page))
                {
                    count = count.checked_add(1).expect("page alias count exhausted");
                    if let Some(leaf) = self.page_table.translate(vpn) {
                        assert_eq!(leaf.ppn(), ppn, "canonical page/leaf identity diverged");
                        self.page_table
                            .revoke_leaf(vpn, &mut commit)
                            .expect("retained private/shared-anonymous swap path");
                    }
                }
            }
            if let Some(shared) = &area.shared_file {
                for (&vpn, resident) in &shared.resident {
                    if resident
                        .mapping
                        .as_ref()
                        .is_some_and(|mapping| mapping.matches(page))
                    {
                        count = count.checked_add(1).expect("page alias count exhausted");
                        if let Some(leaf) = self.page_table.translate(vpn) {
                            assert_eq!(leaf.ppn(), ppn, "canonical page/leaf identity diverged");
                            self.page_table
                                .revoke_leaf(vpn, &mut commit)
                                .expect("retained shared swap path");
                        }
                    }
                }
            }
        }
        commit
            .synchronize()
            .expect("all-CPU fence failed before canonical swap IO");
        self.areas.for_each_mut(|_, area| {
            if let Some(shared) = &mut area.shared_file {
                shared.resident.for_each_mut(|_, resident| {
                    if resident.writer
                        && resident
                            .mapping
                            .as_ref()
                            .is_some_and(|mapping| mapping.matches(page))
                    {
                        resident.page.release_writer();
                        resident.writer = false;
                    }
                });
            }
        });
        count
    }

    pub(super) fn restore_shared_swap_entries(
        &mut self,
        identity: SharedFileId,
    ) -> Result<(), MemoryError> {
        // 已持 MM；迁移只能 try-MM，pin 等待 PageOp 不会与跨 MM blocking callback 成环。
        let page_table = &mut self.page_table;
        self.areas.try_for_each_mut(|_, area| {
            let permission = area.map_permission;
            let Some(shared) = &mut area.shared_file else {
                return Ok(());
            };
            let permission = shared.leaf_permission(permission);
            shared.resident.try_for_each_mut(|&vpn, resident| {
                if resident.is_installed() {
                    return Ok(());
                }
                if !resident
                    .mapping
                    .as_ref()
                    .is_some_and(|mapping| mapping.storage() == Some(identity))
                {
                    return Ok(());
                }
                let page = resident.page.clone();
                let pin = page.pin()?;
                if MapArea::has_leaf_permission(permission) {
                    page_table.reserve(vpn)?;
                    let writer = permission.contains(MapPermission::W);
                    if writer && !resident.writer {
                        page.acquire_writer();
                        resident.writer = true;
                    }
                    let mut commit = TranslationCommit::new();
                    page_table
                        .map(vpn, pin.ppn(), permission.into(), &mut commit)
                        .expect("prepared shared unuse leaf");
                    commit
                        .synchronize()
                        .expect("translation fence failed after shared swapoff unuse");
                }
                resident.mark_installed(&pin);
                Ok(())
            })
        })
    }
}
