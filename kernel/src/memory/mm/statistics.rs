use super::*;

/// Read-only projection of one MM's logical page records, in 4KiB pages.
#[derive(Debug, Default)]
pub(crate) struct UserPageStatistics {
    pub(crate) virtual_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) shared_pages: usize,
    pub(crate) text_pages: usize,
    pub(crate) data_pages: usize,
    /// Linux VmSwap counts private anonymous entries; shmem/tmpfs swap is excluded.
    pub(crate) swapped_pages: usize,
}

impl MemorySet {
    /// 按 Linux `task_statm` 口径投影用户 VMA 页数；不计 kernel-only trap context。
    ///
    /// # Returns
    ///
    /// statm 与 status 同一次遍历的逻辑页快照；physical page-in 不替另一个 alias 增加 RSS。
    pub(crate) fn user_page_statistics(&self) -> UserPageStatistics {
        let text = if self.code_range.is_empty() {
            0
        } else {
            VirtualAddress::from(self.code_range.end).ceil().as_usize()
                - VirtualAddress::from(self.code_range.start)
                    .floor()
                    .as_usize()
        };
        let (size, resident, shared, data, swapped) = self
            .areas
            .values()
            .filter(|area| area.map_permission.contains(MapPermission::U))
            .fold((0, 0, 0, 0, 0), |statistics, area| {
                let (size, resident, shared, data, swapped) = statistics;
                let pages = area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize();
                let shared_file_pages = area.shared_file.as_ref().map_or(0, |shared| {
                    shared
                        .resident
                        .values()
                        .filter(|page| page.is_installed())
                        .count()
                });
                let private_mapping = area.shared_anonymous.is_none();
                let (private_resident_pages, clean_private_file_pages, private_swap_entries) = area
                    .data_frames
                    .iter()
                    .fold((0, 0, 0), |(rss, clean, swap), (&vpn, record)| {
                        let installed = record.is_installed();
                        let file_clean = installed
                            && !record.dirty
                            && area
                                .private_file
                                .as_ref()
                                .is_some_and(|backing| backing.has_file_bytes(vpn));
                        (
                            rss + usize::from(installed),
                            clean + usize::from(file_clean),
                            swap + usize::from(
                                !installed
                                    && private_mapping
                                    && !record.is_file_backed()
                                    && record.mapping().is_some(),
                            ),
                        )
                    });
                let shared_anonymous_pages = if area.shared_anonymous.is_some() {
                    private_resident_pages
                } else {
                    0
                };
                // Linux 只为 `vm_insert_page(s)` 的普通页增加 MM_FILEPAGES；PFNMAP 特殊 PTE 不进 RSS。
                // mprotect(PROT_NONE) 只改写 PTE、不 zap，所以这些页仍计入。
                let device_pages = if area.device.as_ref().is_some_and(DeviceArea::inserts_pages) {
                    pages
                } else {
                    0
                };
                let data_pages = if matches!(area.kind, VmaKind::Stack)
                    || area.map_permission.contains(MapPermission::W)
                        && area.shared_anonymous.is_none()
                        && area.shared_file.is_none()
                        && matches!(area.kind, VmaKind::Anonymous | VmaKind::Elf | VmaKind::File)
                {
                    pages
                } else {
                    0
                };
                (
                    size + pages,
                    resident + private_resident_pages + shared_file_pages + device_pages,
                    shared
                        + shared_anonymous_pages
                        + shared_file_pages
                        + clean_private_file_pages
                        + device_pages,
                    data + data_pages,
                    swapped + private_swap_entries,
                )
            });
        UserPageStatistics {
            virtual_pages: size,
            resident_pages: resident,
            shared_pages: shared,
            text_pages: text,
            data_pages: data,
            swapped_pages: swapped,
        }
    }
}
