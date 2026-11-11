//! A message cache with conservative lifetimes and a byte- and entry-bounded LRU.

use alloc::{sync::Arc, vec::Vec};
use core::cmp::Ordering;

use fallible_tree::{FallibleMap, OutOfMemory};
use packet_wire::dns::{HEADER_BYTES, Header, Message, Section, TYPE_OPT, TYPE_SOA};

use crate::{Answer, Profile, Query};

const MAX_LIFETIME_SECONDS: u32 = 86_400;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    profile: Profile,
    name: Vec<u8>,
}

impl Key {
    fn compare(&self, profile: Profile, name: &[u8]) -> Ordering {
        profile.cmp(&self.profile).then_with(|| {
            name.iter()
                .map(u8::to_ascii_lowercase)
                .cmp(self.name.iter().copied())
        })
    }
}

struct Entry {
    order: u64,
    observed_ms: u64,
    lifetime: u32,
    bytes: Vec<u8>,
    ttl_offsets: Vec<u16>,
    edns_ttl: Option<u32>,
}

impl Entry {
    fn age(&self, now_ms: u64) -> u32 {
        u32::try_from(now_ms.saturating_sub(self.observed_ms) / 1_000).unwrap_or(u32::MAX)
    }

    fn bytes_used(&self, key: &Key) -> usize {
        self.bytes.capacity() + self.ttl_offsets.capacity() * size_of::<u16>() + key.name.capacity()
    }
}

/// Cache one policy's complete recursive responses with monotonically decreasing TTLs.
///
/// Callers replace the cache when upstream policy changes. Question case, DNS transaction ID and
/// TTLs are adapted on a hit. Message-specific EDNS options and authenticated exchanges are skipped.
pub struct Cache {
    entries: FallibleMap<Arc<Key>, Entry>,
    recency: FallibleMap<u64, Arc<Key>>,
    order: u64,
    used: usize,
    entry_limit: usize,
    byte_limit: usize,
}

impl Cache {
    /// Create an empty cache bounded by number of entries and retained packet, name and TTL bytes.
    /// Zero for either bound disables storage. Time arguments use a single monotonic millisecond clock.
    pub fn new(entry_limit: usize, byte_limit: usize) -> Self {
        Self {
            entries: FallibleMap::new(),
            recency: FallibleMap::new(),
            order: 0,
            used: 0,
            entry_limit,
            byte_limit,
        }
    }

    /// Write a current response into caller storage, returning its exact length on a cache hit.
    /// Remove expired entries and apply the same UDP truncation rules as a live answer.
    /// Return a miss if the caller's storage cannot contain the result.
    pub fn answer(
        &mut self,
        query: Query<'_>,
        now_ms: u64,
        udp_limit: Option<usize>,
        output: &mut [u8],
    ) -> Option<usize> {
        let profile = query.profile?;
        let name = query.message.question.name.bytes();
        let entry = self.entries.get_by(|key| key.compare(profile, name))?;
        let age = entry.age(now_ms);
        let order = entry.order;
        if age >= entry.lifetime {
            let key = self
                .recency
                .remove(&order)
                .expect("cache owns one recency node per response");
            self.remove(&key);
            return None;
        }
        let length = if let Some(limit) =
            udp_limit.filter(|&limit| query.needs_tcp(entry.bytes.len(), Some(limit)))
        {
            let header = Header::parse(&entry.bytes).expect("validated cached DNS header");
            query
                .truncated(header, limit, entry.edns_ttl, output)
                .ok()?
        } else {
            let bytes = output.get_mut(..entry.bytes.len())?;
            bytes.copy_from_slice(&entry.bytes);
            bytes[..2].copy_from_slice(&query.header().id.to_be_bytes());
            bytes[HEADER_BYTES..HEADER_BYTES + name.len()].copy_from_slice(name);
            for &offset in &entry.ttl_offsets {
                let field = &mut bytes[usize::from(offset)..][..4];
                field.copy_from_slice(&read_ttl(field).saturating_sub(age).to_be_bytes());
            }
            bytes.len()
        };
        let (slot, _, key) = self
            .recency
            .take_entry(&order)
            .expect("cache owns one recency node per response")
            .into_slot();
        self.order = self
            .order
            .checked_add(1)
            .expect("DNS cache order exhausted");
        self.entries
            .get_mut(&key)
            .expect("recency points to a live response")
            .order = self.order;
        self.recency.commit_vacant(slot.fill(self.order, key));
        Some(length)
    }

    /// Store a verified answer if its lifetime and options permit reuse.
    /// # Errors
    /// Cache memory allocation failed. A caller can still deliver the original response.
    pub fn store(&mut self, answer: Answer<'_, '_>, now_ms: u64) -> Result<(), OutOfMemory> {
        let query = answer.query;
        let Some(profile) = query.profile else {
            return Ok(());
        };
        let message = answer.message;
        if self.entry_limit == 0
            || message.bytes().len() > MAX_MESSAGE_BYTES
            || message.bytes().len() > self.byte_limit
        {
            return Ok(());
        }
        let Some(mut entry) = cacheable(message, profile, now_ms)? else {
            return Ok(());
        };
        let name = query.message.question.name.bytes();
        let mut stored_name = Vec::new();
        stored_name
            .try_reserve_exact(name.len())
            .map_err(|_| OutOfMemory)?;
        stored_name.extend(name.iter().map(u8::to_ascii_lowercase));
        let key = Arc::try_new(Key {
            profile,
            name: stored_name,
        })
        .map_err(|_| OutOfMemory)?;
        let used = entry.bytes_used(&key);
        if used > self.byte_limit {
            return Ok(());
        }
        let order = self
            .order
            .checked_add(1)
            .expect("DNS cache order exhausted");
        entry.order = order;
        entry.edns_ttl = answer.edns_ttl;
        // Reserve both nodes before removing any current cache entries.
        let response_node = FallibleMap::try_prepare(key.clone(), entry)?;
        let recency_node = FallibleMap::try_prepare(order, key.clone())?;
        if let Some(old) = self.entries.get(&key) {
            let old_key = self
                .recency
                .remove(&old.order)
                .expect("cache owns one recency node per response");
            self.remove(&old_key);
        }
        while self.entries.len() >= self.entry_limit || self.used > self.byte_limit - used {
            let (&old, _) = self
                .recency
                .first_key_value()
                .expect("nonempty cache over bound");
            let key = self.recency.remove(&old).expect("oldest cache entry");
            self.remove(&key);
        }
        self.entries.commit_vacant(response_node);
        self.recency.commit_vacant(recency_node);
        self.order = order;
        self.used += used;
        Ok(())
    }

    fn remove(&mut self, key: &Arc<Key>) {
        let old = self
            .entries
            .remove(key)
            .expect("recency points to a live response");
        self.used -= old.bytes_used(key);
    }
}

fn cacheable(
    message: Message<'_>,
    profile: Profile,
    now_ms: u64,
) -> Result<Option<Entry>, OutOfMemory> {
    let header = message.header;
    if !matches!(header.rcode(), 0 | 3) {
        return Ok(None);
    }
    let mut contains_answer = false;
    let mut lifetime = MAX_LIFETIME_SECONDS;
    let mut soa = None;
    let mut edns = false;
    let count = usize::from(header.answers)
        + usize::from(header.authorities)
        + usize::from(header.additional);
    let mut ttl_offsets = Vec::new();
    ttl_offsets
        .try_reserve_exact(count)
        .map_err(|_| OutOfMemory)?;
    for record in message.records() {
        let Ok(record) = record else { return Ok(None) };
        match record.kind {
            TYPE_OPT => {
                if edns
                    || profile.edns_udp_size.is_none()
                    || !record.root_owner
                    || record.section != Section::Additional
                    || record.ttl & !crate::DNSSEC_OK != 0
                    || !record.data.is_empty()
                {
                    return Ok(None);
                }
                edns = true;
            }
            _ => {
                // RFC 2181 treats a set high TTL bit as zero.
                if record.ttl == 0 || record.ttl > i32::MAX as u32 {
                    return Ok(None);
                }
                lifetime = lifetime.min(record.ttl);
                let offset = record.ttl_offset as u16;
                ttl_offsets.push(offset);
                if record.section == Section::Answer
                    && record.class == profile.class
                    && (record.kind == profile.kind || profile.kind == 255)
                {
                    contains_answer = true;
                }
                if record.kind == TYPE_SOA && record.section == Section::Authority {
                    if soa.is_some() || record.class != profile.class {
                        return Ok(None);
                    }
                    soa = Some(record);
                }
            }
        }
    }
    if ttl_offsets.is_empty() {
        return Ok(None);
    }
    let negative_soa = if header.rcode() == 3 || !contains_answer {
        let Some(soa) = soa else { return Ok(None) };
        let Ok(minimum) = soa.soa_minimum() else {
            return Ok(None);
        };
        let ttl = soa.ttl.min(minimum);
        if ttl == 0 {
            return Ok(None);
        }
        lifetime = lifetime.min(ttl);
        Some((soa.ttl_offset, ttl))
    } else {
        None
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(message.bytes().len())
        .map_err(|_| OutOfMemory)?;
    bytes.extend_from_slice(message.bytes());
    if let Some((offset, ttl)) = negative_soa {
        bytes[offset..][..4].copy_from_slice(&ttl.to_be_bytes());
    }
    Ok(Some(Entry {
        order: 0,
        observed_ms: now_ms,
        lifetime,
        bytes,
        ttl_offsets,
        edns_ttl: None,
    }))
}

fn read_ttl(field: &[u8]) -> u32 {
    u32::from_be_bytes(field.try_into().expect("fixed DNS TTL"))
}
