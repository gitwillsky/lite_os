use super::resident::PrivatePagePin;
use super::*;

impl MemorySet {
    /// Pin canonical content after PageOp wait; hold MM through all leaf and epoch publication.
    pub(super) fn handle_private_resident_fault(
        &mut self,
        address: usize,
        access: PageFaultAccess,
        force: bool,
    ) -> Result<bool, MemoryError> {
        let vpn = VirtualAddress::from(address).floor();
        let Some((_, area)) = self.areas.floor_mut(&vpn) else {
            return Ok(false);
        };
        if vpn >= area.vpn_range.end
            || !area.map_permission.contains(MapPermission::U)
            || !force
                && access == PageFaultAccess::Write
                && !area.map_permission.contains(MapPermission::W)
            || area.shared_anonymous.is_some()
            || area.shared_file.is_some()
            || area.device.is_some()
            || area.map_type != MapType::Framed
        {
            return Ok(false);
        }
        let Some(resident) = area.data_frames.get_mut(&vpn) else {
            return Ok(false);
        };
        // 只 clone 内容 owner，不 clone PageMappingRef，避免临时引用改变 COW 独占判定。
        let page = match &resident.backing {
            PrivateBacking::User(mapping) => Some(mapping.page().clone()),
            _ => None,
        };
        let file = match &resident.backing {
            PrivateBacking::File { page, .. } => Some(page.page().clone()),
            _ => None,
        };
        let pin = match (&page, &file) {
            (Some(page), None) => PrivatePagePin::User(page.pin()?),
            (None, Some(file)) => file.pin()?.into(),
            _ => unreachable!("user private content must have one canonical owner"),
        };
        if access != PageFaultAccess::Write {
            if self.page_table.translate(vpn).is_some() {
                TranslationCommit::stale_fault(vpn.as_usize())
                    .synchronize()
                    .expect("retained private read fence");
            } else if MapArea::has_leaf_permission(area.map_permission) {
                let flags = resident.permissions(area.map_permission, area.private_file.is_some());
                let mut commit = TranslationCommit::new();
                self.page_table.map(vpn, pin.ppn(), flags, &mut commit)?;
                commit
                    .synchronize()
                    .expect("retained private publication fence");
            }
            resident.mark_installed(&pin);
            return Ok(true);
        }
        if self
            .page_table
            .translate(vpn)
            .is_some_and(|pte| pte.permissions().contains(PagePermissions::WRITE))
        {
            TranslationCommit::stale_fault(vpn.as_usize())
                .synchronize()
                .expect("stale private write fence");
            return Ok(true);
        }
        let mapped = self.page_table.translate(vpn).is_some();
        let install = MapArea::has_leaf_permission(area.map_permission);
        if !mapped && install {
            self.page_table.reserve(vpn)?;
        }
        if !resident.exclusive() {
            // All allocations and copies precede old leaf retirement. Temporary Page pins do
            // not cause COW; only actual private mapping references decide exclusivity.
            let replacement = pin.copy_frame().ok_or(MemoryError::OutOfMemory)?;
            let ppn = replacement.ppn;
            let replacement = PrivateResident::user(replacement)?;
            if mapped {
                let mut revoke = TranslationCommit::new();
                self.page_table.revoke_leaf(vpn, &mut revoke)?;
                revoke
                    .synchronize()
                    .expect("all-CPU fence before canonical COW replacement");
            }
            let mut publish = TranslationCommit::new();
            if install {
                self.page_table
                    .map(vpn, ppn, area.map_permission.into(), &mut publish)
                    .expect("reserved canonical COW publication");
            }
            publish
                .synchronize()
                .expect("canonical COW publication fence");
            resident.backing = replacement.backing;
        } else {
            let mut commit = TranslationCommit::new();
            if mapped {
                self.page_table
                    .set_flags(vpn, area.map_permission.into(), &mut commit)?;
            } else if install {
                self.page_table
                    .map(vpn, pin.ppn(), area.map_permission.into(), &mut commit)?;
            }
            commit
                .synchronize()
                .expect("canonical private write permission fence");
            resident.mark_installed(&pin);
        }
        resident.discardable = false;
        resident.dirty |= area.private_file.is_some();
        Ok(true)
    }
}
