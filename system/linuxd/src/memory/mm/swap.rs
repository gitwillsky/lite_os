//! Canonical Page physical IO runs outside MM; this file owns logical swapoff installation only.
use super::*;

impl MemorySet {
    /// Restore every matching logical entry. PROT_NONE installs the epoch without publishing a leaf.
    pub(crate) fn restore_swapped_pages(
        &mut self,
        identity: SharedFileId,
    ) -> Result<(), MemoryError> {
        let page_table = &mut self.page_table;
        self.areas.try_for_each_mut(|_, area| {
            let permission = area.map_permission;
            let shared = area.shared_anonymous.is_some();
            let private_file = area.private_file.is_some();
            area.data_frames.try_for_each_mut(|&vpn, resident| {
                let Some(mapping) = resident.mapping() else {
                    return Ok(());
                };
                if mapping.is_installed() || mapping.storage() != Some(identity) {
                    return Ok(());
                }
                let page = mapping.page().clone();
                let pin = page.pin()?;
                if MapArea::has_leaf_permission(permission) {
                    page_table.reserve(vpn)?;
                    let flags = if shared {
                        permission.into()
                    } else {
                        resident.permissions(permission, private_file)
                    };
                    let mut commit = TranslationCommit::new();
                    page_table
                        .map(vpn, pin.ppn(), flags, &mut commit)
                        .expect("prepared canonical swapoff leaf");
                    commit
                        .synchronize()
                        .expect("canonical swapoff publication fence");
                }
                if let Some(mapping) = resident.mapping_mut() {
                    mapping.mark_installed(&pin);
                }
                Ok::<(), MemoryError>(())
            })
        })?;
        self.restore_shared_swap_entries(identity)
    }
}
