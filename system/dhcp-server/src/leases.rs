//! A single pool owns address occupancy, client identity, and the next lease deadline.

use alloc::{sync::Arc, vec::Vec};
use fallible_tree::{FallibleMap, OutOfMemory};

const OFFER_MS: u64 = 60_000;
const DECLINE_MS: u64 = 600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Checking,
    Offered(u32),
    Bound,
    Quarantined,
}

struct Lease {
    owner: Option<Arc<Vec<u8>>>,
    mac: [u8; 6],
    expires: u64,
    phase: Phase,
}

pub(super) struct Leases {
    first: u32,
    last: u32,
    // Rotating allocation start avoids rescanning a growing run for sequential allocations.
    cursor: u32,
    // Leases excluded by a range edit still occupy their old addresses until expiry.
    // Count them separately to preserve constant-time full-pool detection.
    outside: usize,
    addresses: FallibleMap<u32, Lease>,
    clients: FallibleMap<Arc<Vec<u8>>, u32>,
    deadlines: FallibleMap<(u64, u32), ()>,
}

impl Leases {
    pub(super) fn new(first: u32, last: u32) -> Self {
        Self {
            first,
            last,
            cursor: first,
            outside: 0,
            addresses: FallibleMap::new(),
            clients: FallibleMap::new(),
            deadlines: FallibleMap::new(),
        }
    }

    pub(super) fn expiry(&self) -> Option<u64> {
        self.deadlines
            .first_key_value()
            .map(|(&(expires, _), _)| expires)
    }

    pub(super) fn expire(&mut self, now: u64) {
        while let Some((&(expires, address), _)) = self.deadlines.first_key_value() {
            if expires > now {
                break;
            }
            self.remove(address);
        }
    }

    pub(super) fn lookup(&self, client: &[u8]) -> Option<(u32, Phase)> {
        let &address = self.clients.get_by(|key| client.cmp(key.as_slice()))?;
        let phase = self
            .addresses
            .get(&address)
            .expect("client index follows address")
            .phase;
        Some((address, phase))
    }

    pub(super) fn bound(&self, after: u32) -> impl Iterator<Item = (u32, [u8; 6], u64)> + '_ {
        self.addresses
            .iter_after(&after)
            .filter(|(_, lease)| lease.phase == Phase::Bound)
            .map(|(&address, lease)| (address, lease.mac, lease.expires))
    }

    pub(super) fn offer(
        &mut self,
        client: &[u8],
        mac: [u8; 6],
        phase: Phase,
        preferred: Option<u32>,
        now: u64,
    ) -> Result<Option<u32>, OutOfMemory> {
        let expiry = if phase == Phase::Checking {
            // The probe timer owns the reservation until it sends an offer or cancels.
            u64::MAX
        } else {
            now.saturating_add(OFFER_MS)
        };
        if let Some((address, phase)) = self.lookup(client) {
            if matches!(phase, Phase::Offered(_)) {
                self.update(address, mac, phase, expiry);
            }
            return Ok(Some(address));
        }
        let preferred = preferred.filter(|address| {
            (self.first..=self.last).contains(address) && self.addresses.get(address).is_none()
        });
        let Some(address) = preferred.or_else(|| self.next_free()) else {
            return Ok(None);
        };
        let mut identity = Vec::new();
        identity
            .try_reserve_exact(client.len())
            .map_err(|_| OutOfMemory)?;
        identity.extend_from_slice(client);
        let owner = Arc::try_new(identity).map_err(|_| OutOfMemory)?;
        let client = self.clients.try_prepare_vacant(owner.clone(), address)?;
        let deadline = self.deadlines.try_prepare_vacant((expiry, address), ())?;
        let lease = self.addresses.try_prepare_vacant(
            address,
            Lease {
                owner: Some(owner),
                mac,
                expires: expiry,
                phase,
            },
        )?;
        // All three indices are reserved before any change can be observed or fail.
        self.addresses.commit_vacant(lease);
        self.clients.commit_vacant(client);
        self.deadlines.commit_vacant(deadline);
        self.cursor = if address == self.last {
            self.first
        } else {
            address + 1
        };
        Ok(Some(address))
    }

    pub(super) fn set_range(&mut self, first: u32, last: u32, same_link: bool) {
        if same_link && first == self.first && last == self.last {
            return;
        }
        let mut previous = 0;
        let range = first..=last;
        while let Some((&address, lease)) = self.addresses.iter_after(&previous).next() {
            previous = address;
            if same_link && range.contains(&address) {
                continue;
            }
            if matches!(lease.phase, Phase::Checking | Phase::Offered(_)) {
                self.remove(address);
            } else {
                let lease = self.addresses.get_mut(&address).expect("indexed address");
                if let Some(owner) = lease.owner.take() {
                    self.clients.remove(&owner);
                }
                lease.phase = Phase::Quarantined;
            }
        }
        self.outside = self
            .addresses
            .iter()
            .filter(|(address, _)| !range.contains(address))
            .count();
        self.first = first;
        self.last = last;
        self.cursor = self.cursor.clamp(first, last);
    }

    fn next_free(&self) -> Option<u32> {
        if (self.addresses.len() - self.outside) as u64
            == u64::from(self.last) - u64::from(self.first) + 1
        {
            return None;
        }
        let gap = |first, last| {
            let mut candidate = first;
            for (&used, _) in self.addresses.iter_from(&first) {
                if candidate > last {
                    return None;
                }
                if used != candidate {
                    return Some(candidate);
                }
                if used == last {
                    return None;
                }
                candidate += 1;
            }
            (candidate <= last).then_some(candidate)
        };
        gap(self.cursor, self.last).or_else(|| {
            (self.cursor > self.first)
                .then(|| gap(self.first, self.cursor - 1))
                .flatten()
        })
    }

    pub(super) fn checked(&mut self, address: u32, mac: [u8; 6], xid: u32, now: u64) {
        self.update(
            address,
            mac,
            Phase::Offered(xid),
            now.saturating_add(OFFER_MS),
        );
    }

    pub(super) fn bind(&mut self, address: u32, mac: [u8; 6], expires: u64) {
        self.update(address, mac, Phase::Bound, expires);
    }

    pub(super) fn decline(&mut self, address: u32, mac: [u8; 6], now: u64) {
        self.update(
            address,
            mac,
            Phase::Quarantined,
            now.saturating_add(DECLINE_MS),
        );
        let lease = self
            .addresses
            .get_mut(&address)
            .expect("declined occupied address");
        if let Some(owner) = lease.owner.take() {
            self.clients.remove(&owner);
        }
    }

    fn update(&mut self, address: u32, mac: [u8; 6], phase: Phase, expires: u64) {
        let lease = self.addresses.get_mut(&address).expect("occupied address");
        if lease.expires != expires {
            let (slot, _, ()) = self
                .deadlines
                .take_entry(&(lease.expires, address))
                .expect("lease owns a deadline")
                .into_slot();
            self.deadlines
                .commit_vacant(slot.fill((expires, address), ()));
        }
        lease.mac = mac;
        lease.expires = expires;
        lease.phase = phase;
    }

    pub(super) fn remove(&mut self, address: u32) {
        if let Some(lease) = self.addresses.remove(&address) {
            if !(self.first..=self.last).contains(&address) {
                self.outside -= 1;
            }
            self.deadlines.remove(&(lease.expires, address));
            if let Some(owner) = lease.owner {
                self.clients.remove(&owner);
            }
        }
    }
}
