use super::*;

impl MemorySet {
    /// @description 按 Linux `task_statm` 口径投影用户 VMA 页数；不计 kernel-only trap context。
    /// @return `(size, resident, shared, text, data)`，单位均为页。
    pub(crate) fn user_page_statistics(&self) -> (usize, usize, usize, usize, usize) {
        let text = if self.code_range.is_empty() {
            0
        } else {
            VirtualAddress::from(self.code_range.end).ceil().as_usize()
                - VirtualAddress::from(self.code_range.start)
                    .floor()
                    .as_usize()
        };
        let (size, resident, shared, data) = self
            .areas
            .values()
            .filter(|area| area.map_permission.contains(MapPermission::U))
            .fold((0, 0, 0, 0), |statistics, area| {
                let (size, resident, shared, data) = statistics;
                let pages = area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize();
                let shared_file_pages = area
                    .shared_file
                    .as_ref()
                    .map_or(0, |shared| shared.resident.len());
                let private_resident_pages = area.data_frames.len();
                let clean_private_file_pages = area.private_file.as_ref().map_or(0, |backing| {
                    area.data_frames
                        .iter()
                        .filter(|(vpn, resident)| backing.has_file_bytes(**vpn) && !resident.dirty)
                        .count()
                });
                let shared_anonymous_pages = if area.shared_anonymous.is_some() {
                    private_resident_pages
                } else {
                    0
                };
                let device_pages =
                    if area.device.is_some() && MapArea::has_leaf_permission(area.map_permission) {
                        pages
                    } else {
                        0
                    };
                let data_pages = if matches!(area.kind, VmaKind::Stack { .. })
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
                )
            });
        (size, resident, shared, text, data)
    }
}
