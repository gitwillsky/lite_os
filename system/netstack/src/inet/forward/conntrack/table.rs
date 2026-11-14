//! 连接原键和线上键在固定执行分区内各有一个位置。相反方向先规范化为同一个键。

use super::{Datagram, Flow, FlowHash, FlowObservation, Key, Protocol};
use crate::sync::Mutex;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hash::BuildHasher;
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::HashTable;
use ::tcp::time::Instant;

mod deadlines;
use deadlines::{DeadlineQueue, NO_SHARD};

struct Index<V>(HashTable<(Key, V)>);

impl<V> Index<V> {
    const fn new() -> Self {
        Self(HashTable::new())
    }

    fn get(&self, key: Key, hash: u64) -> Option<&V> {
        self.0
            .find(hash, |entry| entry.0 == key)
            .map(|entry| &entry.1)
    }

    fn reserve(&mut self, hasher: FlowHash) -> Option<()> {
        self.0.try_reserve(1, |entry| hasher.hash_one(entry.0)).ok()
    }

    fn insert(&mut self, key: Key, value: V, hash: u64, hasher: FlowHash) {
        debug_assert!(self.get(key, hash).is_none());
        assert!(
            self.0.len() < self.0.capacity(),
            "connection index capacity must be reserved before TX"
        );
        self.0
            .insert_unique(hash, (key, value), |entry| hasher.hash_one(entry.0));
    }

    fn remove(&mut self, key: Key, hash: u64) -> Option<V> {
        self.0
            .find_entry(hash, |entry| entry.0 == key)
            .ok()
            .map(|entry| entry.remove().0.1)
    }

    fn release_empty(&mut self) {
        if self.0.is_empty() && self.0.capacity() != 0 {
            *self = Self::new();
        }
    }
}

struct Shard {
    deadlines: DeadlineQueue,
    // 原键拥有映射，线上键只负责定位原键。两者可落在不同分区。
    outgoing: Index<Datagram>,
    incoming: Index<Key>,
}

impl Shard {
    fn new() -> Self {
        Self {
            deadlines: DeadlineQueue::new(),
            outgoing: Index::new(),
            incoming: Index::new(),
        }
    }
}

/// 一次线上键查找取得的连接和转换事实，只在当前域 guard 内使用。
#[derive(Default)]
pub(super) struct Lookup {
    pub(super) flow: Option<FlowObservation>,
    pub(super) outgoing: Option<Datagram>,
    pub(super) incoming: Option<Key>,
}

/// 域内连接索引。固定分区数来自启动时的 CPU 拓扑，专用核调度变更不会重分配键。
/// 原键和线上键分别用同一 SipHash 的中间位选分区，低位和高位仍用于 SwissTable 的桶与标签。
pub(super) struct Tables {
    hash: FlowHash,
    // 首次跟踪才装配；分区版本在域生命周期内保留，避免旧 RX 结果被误认。
    shared: Option<Arc<FlowShards>>,
    shards: Vec<Shard>,
    // 最小值树只保存候选分区编号。叶子对应固定分区，根可直接定位最早截止。
    deadline_winners: Vec<usize>,
    retire_from: usize,
    translated: [usize; 2],
}

impl Tables {
    pub(super) fn new(hash: FlowHash) -> Self {
        Self {
            hash,
            shared: None,
            shards: Vec::new(),
            deadline_winners: Vec::new(),
            retire_from: 0,
            translated: [0; 2],
        }
    }

    pub(super) fn hasher(&self) -> FlowHash {
        self.hash
    }

    pub(super) fn hash_key(&self, key: Key) -> u64 {
        self.hash.hash_one(key)
    }

    fn prepare(&mut self) -> Option<()> {
        if self.shards.is_empty() {
            let width = crate::worker::partition_count();
            let nodes = width.checked_next_power_of_two()?.checked_mul(2)?;
            self.shards.try_reserve_exact(width).ok()?;
            self.deadline_winners.try_reserve_exact(nodes).ok()?;
            if self.shared.is_none() {
                self.shared = Some(FlowShards::prepare(width)?);
            }
            self.deadline_winners.resize(nodes, NO_SHARD);
            self.shards.resize_with(width, Shard::new);
        }
        Some(())
    }

    fn slot(&self, hash: u64) -> usize {
        slot(hash, self.shards.len())
    }

    fn location(&self, key: Key) -> (usize, u64) {
        let hash = self.hash.hash_one(key);
        (self.slot(hash), hash)
    }

    fn at_mut(&mut self, key: Key) -> (&mut Shard, u64) {
        let (slot, hash) = self.location(key);
        (&mut self.shards[slot], hash)
    }

    pub(super) fn flow(&self, key: Key) -> Option<FlowObservation> {
        if self.shards.is_empty() {
            return None;
        }
        self.hashed_flow(key, self.hash_key(key))
    }

    pub(super) fn hashed_flow(&self, key: Key, hash: u64) -> Option<FlowObservation> {
        self.read_flow(key, hash, FlowObservation::capture)
    }

    pub(super) fn with_flow<R>(&self, key: Key, read: impl FnOnce(&Flow) -> R) -> Option<R> {
        self.read_flow(key, self.hash_key(key), read)
    }

    fn read_flow<R>(&self, key: Key, hash: u64, read: impl FnOnce(&Flow) -> R) -> Option<R> {
        let flows = self.shared.as_ref()?.for_hash(hash).flows.lock();
        flows.get(key, hash).map(read)
    }

    pub(super) fn lookup(&self, key: Key, hash: u64) -> Lookup {
        let Some(shard) = self.shards.get(self.slot(hash)) else {
            return Lookup::default();
        };
        let translated = self.translates(key.endpoints[0].0.is_ipv6());
        if translated && let Some(&original) = shard.incoming.get(key, hash) {
            // 线上别名与原连接键互斥，命中别名后无需探测另两张表。
            return Lookup {
                incoming: Some(original),
                ..Lookup::default()
            };
        }
        let flow = self.hashed_flow(key, hash);
        let outgoing = if translated && flow.is_some() {
            shard.outgoing.get(key, hash).copied()
        } else {
            None
        };
        Lookup {
            flow,
            outgoing,
            incoming: None,
        }
    }

    pub(super) fn edit_flow<R>(
        &self,
        key: Key,
        hash: u64,
        edit: impl FnOnce(usize, &mut Flow) -> R,
    ) -> Option<R> {
        let slot = self.slot(hash);
        let shard = self.shared.as_ref()?.for_hash(hash);
        let mut flows = shard.flows.lock();
        let entry = flows.0.find_mut(hash, |entry| entry.0 == key)?;
        let flow = &mut entry.1;
        let previous = (
            flow.generation,
            flow.expires,
            flow.original_reversed,
            flow.replied,
        );
        let result = edit(slot, flow);
        if key.protocol == Protocol::Udp
            && (flow.generation != previous.0
                || flow.expires < previous.1
                || flow.original_reversed != previous.2
                || flow.replied != previous.3)
        {
            shard.changed();
        }
        Some(result)
    }

    pub(super) fn reserve_flow(&mut self, hash: u64) -> Option<()> {
        self.prepare()?;
        self.shared
            .as_ref()?
            .for_hash(hash)
            .flows
            .lock()
            .reserve(self.hash)
    }

    pub(super) fn insert_flow(&mut self, key: Key, hash: u64, flow: Flow) -> usize {
        let slot = self.slot(hash);
        let shard = self.shared.as_ref().expect("reserved shard").for_hash(hash);
        let mut flows = shard.flows.lock();
        flows.insert(key, flow, hash, self.hash);
        if key.protocol == Protocol::Udp {
            shard.changed();
        }
        slot
    }

    pub(super) fn remove_flow(&mut self, key: Key) {
        let hash = self.hash_key(key);
        let shard = self.shared.as_ref().expect("indexed shard").for_hash(hash);
        let mut flows = shard.flows.lock();
        assert!(flows.remove(key, hash).is_some());
        flows.release_empty();
        if key.protocol == Protocol::Udp {
            shard.changed();
        }
        drop(flows);
        self.replace_translation(key, None);
    }

    pub(super) fn reset_empty(&mut self) {
        debug_assert_eq!(self.translated, [0; 2]);
        debug_assert!(self.next_deadline().is_none());
        self.shards = Vec::new();
        self.deadline_winners = Vec::new();
        self.retire_from = 0;
    }

    pub(super) fn translates(&self, ipv6: bool) -> bool {
        self.translated[usize::from(ipv6)] != 0
    }

    pub(super) fn outgoing(&self, original: Key) -> Option<Datagram> {
        if !self.translates(original.endpoints[0].0.is_ipv6()) {
            return None;
        }
        self.hashed_outgoing(original, self.hash_key(original))
    }

    pub(super) fn hashed_outgoing(&self, original: Key, hash: u64) -> Option<Datagram> {
        if !self.translates(original.endpoints[0].0.is_ipv6()) {
            return None;
        }
        self.shards
            .get(self.slot(hash))?
            .outgoing
            .get(original, hash)
            .copied()
    }

    pub(super) fn incoming(&self, wire: Key) -> Option<Key> {
        if !self.translates(wire.endpoints[0].0.is_ipv6()) {
            return None;
        }
        self.hashed_incoming(wire, self.hash_key(wire))
    }

    pub(super) fn hashed_incoming(&self, wire: Key, hash: u64) -> Option<Key> {
        if !self.translates(wire.endpoints[0].0.is_ipv6()) {
            return None;
        }
        self.shards
            .get(self.slot(hash))?
            .incoming
            .get(wire, hash)
            .copied()
    }

    pub(super) fn reserve_translation(&mut self, original: Key, wire: Key) -> Option<()> {
        self.prepare()?;
        let hasher = self.hash;
        self.at_mut(original).0.outgoing.reserve(hasher)?;
        self.at_mut(wire).0.incoming.reserve(hasher)
    }

    pub(super) fn install_translation(&mut self, original: Key, wire: Datagram) {
        let hasher = self.hash;
        let (shard, hash) = self.at_mut(original);
        shard.outgoing.insert(original, wire, hash, hasher);
        let (shard, hash) = self.at_mut(wire.key);
        shard.incoming.insert(wire.key, original, hash, hasher);
        self.translated[usize::from(original.endpoints[0].0.is_ipv6())] += 1;
    }

    pub(super) fn replace_translation(&mut self, original: Key, wire: Option<Datagram>) {
        let (shard, hash) = self.at_mut(original);
        let previous = shard.outgoing.remove(original, hash);
        if let Some(previous) = previous {
            let (shard, hash) = self.at_mut(previous.key);
            assert!(shard.incoming.remove(previous.key, hash) == Some(original));
            self.translated[usize::from(original.endpoints[0].0.is_ipv6())] -= 1;
        }
        if let Some(wire) = wire {
            self.install_translation(original, wire);
        }
        // 新旧线上键可能落在同一分区。提交新映射之后才能释放旧索引容量。
        if let Some(previous) = previous {
            self.at_mut(original).0.outgoing.release_empty();
            self.at_mut(previous.key).0.incoming.release_empty();
        }
    }
}

struct FlowShard {
    // OWNER: 唯一原始连接表。域写者持域 guard，队列读者只 try_lock 本分区。
    flows: Mutex<Index<Flow>>,
    // OWNER: 写者在分区锁内推进。旧版本不允许消费；0 退休版本空间。
    revision: AtomicU64,
}

impl FlowShard {
    fn changed(&self) {
        let current = self.revision.load(Ordering::Relaxed);
        if current != 0 {
            self.revision
                .store(current.checked_add(1).unwrap_or(0), Ordering::Release);
        }
    }
}

pub(super) struct FlowShards(Vec<FlowShard>);

impl FlowShards {
    fn prepare(width: usize) -> Option<Arc<Self>> {
        let mut shards = Vec::new();
        shards.try_reserve_exact(width).ok()?;
        shards.resize_with(width, || FlowShard {
            flows: Mutex::new(Index::new()),
            revision: AtomicU64::new(1),
        });
        Arc::try_new(Self(shards)).ok()
    }

    fn for_hash(&self, hash: u64) -> &FlowShard {
        &self.0[slot(hash, self.0.len())]
    }

    pub(super) fn probe(&self, key: Key, hash: u64) -> Option<[u64; 3]> {
        debug_assert!(key.protocol == Protocol::Udp);
        let shard = self.for_hash(hash);
        let flows = shard.flows.try_lock()?;
        let revision = shard.revision.load(Ordering::Relaxed);
        if revision == 0 {
            return None;
        }
        let Some(flow) = flows.get(key, hash) else {
            return Some([revision, 0, 0]);
        };
        if flow.generation > u64::MAX >> 2 {
            return None;
        }
        let flags = (u64::from(flow.replied) << 1) | u64::from(flow.original_reversed);
        Some([
            revision,
            (flow.generation << 2) | flags,
            flow.expires.total_micros() as u64,
        ])
    }
}

impl Tables {
    pub(super) fn shared(&self) -> Option<&Arc<FlowShards>> {
        self.shared.as_ref()
    }

    /// 域 guard 排斥写者。旧期限尚未到达时，单调延长仍可使用该提示。
    pub(super) fn validate_probe(
        &self,
        hash: u64,
        [revision, state, expires]: [u64; 3],
        now: Instant,
    ) -> Option<Option<FlowObservation>> {
        if revision == 0
            || self
                .shared
                .as_ref()?
                .for_hash(hash)
                .revision
                .load(Ordering::Acquire)
                != revision
        {
            return None;
        }
        if state == 0 {
            return Some(None);
        }
        let expires = Instant::from_micros(expires as i64);
        if expires <= now {
            return None;
        }
        Some(Some(FlowObservation {
            generation: state >> 2,
            expires,
            original_reversed: state & 1 != 0,
            replied: state & 2 != 0,
            tcp: None,
        }))
    }
}

fn slot(hash: u64, width: usize) -> usize {
    let selector = u64::from((hash >> 24) as u32);
    ((selector * width as u64) >> 32) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inet::forward::conntrack::Protocol;
    use core::net::Ipv4Addr;
    use ::tcp::time::Instant;

    #[test]
    fn queued_udp_results_are_checked_against_live_flow_changes() {
        use crate::inet::forward::conntrack::{Flows, Packet, ReceiveFlow};
        let datagram = Datagram::ports(
            Protocol::Udp,
            (Ipv4Addr::new(192, 0, 2, 1).into(), 45123),
            (Ipv4Addr::new(198, 51, 100, 2).into(), 53),
        )
        .unwrap();
        let mut flows = Flows::new();
        let key = datagram.key;
        let hash = flows.tables.hash_key(key);
        let start = Instant::from_millis(1);
        let prepare = |flows: &mut Flows, packet, hint, now| {
            let (_, tracked) = flows
                .observe(Packet::Data(packet), hint, now)
                .unwrap()
                .unwrap();
            flows.reserve(tracked, now).unwrap()
        };
        let tracked = prepare(&mut flows, datagram, None, start);
        let shards = flows.tables.shared().unwrap().clone();
        let held = shards.for_hash(hash).flows.lock();
        assert!(shards.probe(key, hash).is_none());
        drop(held);
        let miss = shards.probe(key, hash).unwrap();
        assert!(matches!(
            flows.tables.validate_probe(hash, miss, start),
            Some(None)
        ));
        flows.confirm(tracked, start);
        assert!(flows.tables.validate_probe(hash, miss, start).is_none());

        let observation = shards.probe(key, hash).unwrap();
        let hint = Some(ReceiveFlow { hash, observation });
        let later = Instant::from_millis(1_001);
        let tracked = prepare(&mut flows, datagram, hint, later);
        flows.confirm(tracked, later);
        assert!(matches!(
            flows.tables.validate_probe(hash, observation, later),
            Some(Some(_))
        ));
        let old_expiry = Instant::from_millis(30_001);
        assert!(
            flows
                .tables
                .validate_probe(hash, observation, old_expiry)
                .is_none()
        );
        assert!(
            prepare(&mut flows, datagram, hint, old_expiry)
                .existing_generation()
                .is_some()
        );

        let reply = Datagram {
            reversed: !datagram.reversed,
            ..datagram
        };
        let tracked = prepare(&mut flows, reply, hint, later);
        flows.confirm(tracked, later);
        assert!(
            flows
                .tables
                .validate_probe(hash, observation, later)
                .is_none()
        );
        let latest = shards.probe(key, hash).unwrap();
        let id = latest[1] >> 2;
        flows
            .clear(
                router_protocol::connections::Identity {
                    epoch: flows.epoch,
                    id,
                },
                later,
            )
            .unwrap();
        assert!(flows.tables.validate_probe(hash, latest, later).is_none());
        assert!(Arc::ptr_eq(flows.tables.shared().unwrap(), &shards));
    }

    #[test]
    fn nat_alias_replacement_keeps_reserved_capacity_across_shards() {
        let mut tables = Tables::new(FlowHash([0x3d; 16]));
        let datagram = |port| {
            Datagram::ports(
                Protocol::Udp,
                (Ipv4Addr::new(192, 0, 2, 1).into(), port),
                (Ipv4Addr::new(198, 51, 100, 2).into(), 53),
            )
            .unwrap()
        };
        let original = datagram(1000).key;
        let original_hash = tables.hash_key(original);
        tables.reserve_flow(original_hash).unwrap();
        let origin_slot = tables.slot(tables.hasher().hash_one(original));
        let mut candidates = (1001..=2000)
            .map(datagram)
            .filter(|wire| tables.slot(tables.hasher().hash_one(wire.key)) != origin_slot);
        let previous = candidates.next().expect("another shard");
        let previous_slot = tables.slot(tables.hasher().hash_one(previous.key));
        let replacement = candidates
            .find(|wire| tables.slot(tables.hasher().hash_one(wire.key)) == previous_slot)
            .expect("replacement on the old alias shard");

        let expires = Instant::from_millis(1000);
        tables.insert_flow(
            original,
            original_hash,
            Flow {
                generation: 1,
                expires,
                scheduled: expires,
                original_reversed: false,
                replied: false,
                tcp: None,
            },
        );
        tables.reserve_translation(original, previous.key).unwrap();
        tables.install_translation(original, previous);
        assert!(
            tables
                .lookup(previous.key, tables.hash_key(previous.key))
                .incoming
                == Some(original)
        );
        tables
            .reserve_translation(original, replacement.key)
            .unwrap();
        tables.replace_translation(original, Some(replacement));
        assert!(
            tables
                .lookup(previous.key, tables.hash_key(previous.key))
                .incoming
                .is_none()
        );
        assert!(
            tables
                .lookup(replacement.key, tables.hash_key(replacement.key))
                .incoming
                == Some(original)
        );
        assert!(tables.lookup(original, tables.hash_key(original)).outgoing == Some(replacement));
        tables.remove_flow(original);
        assert!(
            tables
                .lookup(replacement.key, tables.hash_key(replacement.key))
                .incoming
                .is_none()
        );
        assert!(!tables.translates(false));
    }
}
