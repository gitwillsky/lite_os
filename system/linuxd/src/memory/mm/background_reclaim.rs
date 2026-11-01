//! Registered MM selects candidates while locked; canonical Page performs IO after guard release.
use super::*;
use crate::memory::SwappablePage;

/// One bounded round across private, shared-anonymous and shared-file mapping records.
pub(crate) struct BackgroundReclaimScan(PrivateReclaimWalk);

/// Exactly one scanned record; clean retirement is completed under MM, physical migration follows.
pub(crate) enum BackgroundReclaimCandidate {
    Clean(bool),
    Swap(Arc<SwappablePage>),
    Skip,
}

impl MapArea {
    fn next_background_record(&self, cursor: VirtualPageNumber) -> Option<VirtualPageNumber> {
        let private = self.data_frames.ceiling(&cursor).map(|(&vpn, _)| vpn);
        let shared = self
            .shared_file
            .as_ref()
            .and_then(|shared| shared.resident.ceiling(&cursor))
            .map(|(&vpn, _)| vpn);
        match (private, shared) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl MemorySet {
    pub(crate) fn begin_background_reclaim(&self) -> BackgroundReclaimScan {
        BackgroundReclaimScan(PrivateReclaimWalk::new(
            self.private_reclaim_cursor.as_usize(),
        ))
    }

    fn next_background_record(
        &self,
        cursor: VirtualPageNumber,
    ) -> Option<(VirtualPageNumber, VirtualPageNumber)> {
        let previous = self.areas.floor(&cursor);
        if let Some((&key, area)) = previous
            && cursor < area.vpn_range.end
            && let Some(vpn) = area.next_background_record(cursor)
        {
            return Some((key, vpn));
        }
        let mut remaining = match previous {
            Some((&key, _)) => self.areas.iter_after(&key),
            None => self.areas.iter_from(&cursor),
        };
        remaining.find_map(|(&key, area)| {
            area.next_background_record(VirtualPageNumber::from_vpn(0))
                .map(|vpn| (key, vpn))
        })
    }

    /// No IO/allocation here. Caller releases MM before executing a Swap candidate.
    pub(crate) fn next_background_reclaim(
        &mut self,
        scan: &mut BackgroundReclaimScan,
    ) -> Option<BackgroundReclaimCandidate> {
        let (key, vpn) = loop {
            if let Some(next) =
                self.next_background_record(VirtualPageNumber::from_vpn(scan.0.probe()))
            {
                break next;
            }
            if !scan.0.wrap_or_finish() {
                return None;
            }
        };
        if !scan.0.advance(vpn.as_usize()) {
            return None;
        }
        self.private_reclaim_cursor = VirtualPageNumber::from_vpn(scan.0.committed());
        if self
            .private_reclaim_exclusion
            .as_ref()
            .is_some_and(|range| range.contains(&vpn))
        {
            return Some(BackgroundReclaimCandidate::Skip);
        }
        let area = self.areas.get(&key).expect("background scan VMA");
        if !area.map_permission.contains(MapPermission::U) || area.device.is_some() {
            return Some(BackgroundReclaimCandidate::Skip);
        }
        if let Some(resident) = area.data_frames.get(&vpn) {
            if resident.discardable || area.private_file.is_some() && !resident.dirty {
                self.private_reclaim_cursor = vpn;
                let mut request = ReclaimRequest::for_target(1);
                request.limit_scans(1);
                let result = self.reclaim_clean_private_pages(request);
                assert_eq!(
                    result.scanned_pages(),
                    1,
                    "single background clean candidate scan"
                );
                return Some(BackgroundReclaimCandidate::Clean(
                    result.reclaimed_pages() != 0,
                ));
            }
            return Some(
                resident
                    .mapping()
                    .map_or(BackgroundReclaimCandidate::Skip, |mapping| {
                        BackgroundReclaimCandidate::Swap(mapping.page().clone())
                    }),
            );
        }
        let resident = area
            .shared_file
            .as_ref()
            .expect("background shared VMA")
            .resident
            .get(&vpn)
            .expect("background shared record");
        Some(
            resident
                .mapping
                .as_ref()
                .map_or(BackgroundReclaimCandidate::Skip, |mapping| {
                    BackgroundReclaimCandidate::Swap(mapping.page().clone())
                }),
        )
    }
}

/// Registered address-space adapter; IO always follows release of the originating MM guard.
pub(crate) fn reclaim_address_space(
    memory_set: &crate::execution::sync::TaskMutex<MemorySet>,
    request: ReclaimRequest,
) -> ReclaimResult {
    if !request.allows_swap_io() {
        return memory_set
            .try_lock()
            .map_or_else(ReclaimResult::default, |mut memory| {
                memory.reclaim_private_pages(request)
            });
    }
    let Some(mut scan) = memory_set
        .try_lock()
        .map(|memory| memory.begin_background_reclaim())
    else {
        return ReclaimResult::default();
    };
    let mut scanned = 0;
    let mut reclaimed = 0;
    while scanned < request.scan_pages() && reclaimed < request.target_pages() {
        let candidate = {
            let Some(mut memory) = memory_set.try_lock() else {
                break;
            };
            memory.next_background_reclaim(&mut scan)
        };
        let Some(candidate) = candidate else {
            break;
        };
        scanned += 1;
        reclaimed += usize::from(match candidate {
            BackgroundReclaimCandidate::Clean(reclaimed) => reclaimed,
            BackgroundReclaimCandidate::Swap(page) => page.swap_out(),
            BackgroundReclaimCandidate::Skip => false,
        });
    }
    ReclaimResult::new(reclaimed, scanned)
}
