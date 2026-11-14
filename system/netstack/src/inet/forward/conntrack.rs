//! 双栈传输共用双向连接跟踪和定时清理。正反方向共用一个表项，只有实际提交转发才确认或刷新状态。

mod nat;
mod packet;
mod policy;
mod table;
mod tcp;
pub(super) use nat::{EndpointEdits, TranslationEdit};
pub(super) use packet::{Datagram, Key, Packet};
pub(in crate::inet) use packet::prepare_transport;
use packet::{Endpoint, Protocol, trackable_addresses};
pub(in crate::inet) use policy::{PolicySlot, RetainedPolicies};
use table::{FlowShards, Tables};

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{hash::BuildHasher, net::SocketAddr};
use fallible_tree::{FallibleMap, VacantEntry};
use native_runtime::{
    Event,
    native_abi::{self, Status, signals},
};
use router_protocol::connections::changes::{self, Change};
use router_protocol::{connections, firewall::ConnectionState};
use siphasher::sip::SipHasher13;
use ::tcp::time::{Duration, Instant};

const CAPACITY: usize = 1_000_000;
const RETIRE_BATCH: usize = 64;
// 每八条截止记录检查一次软截止，限制并摊薄时钟读取。轮次至少推进一组。
const RETIRE_CLOCK_INTERVAL: usize = 8;
const SHORT_IDLE: Duration = Duration::from_millis(30_000);
const UDP_REPLIED: Duration = Duration::from_millis(120_000);
const CHANGE_HISTORY: usize = 1024;

#[derive(Clone, Copy)]
pub(super) enum Observation {
    Data {
        datagram: Datagram,
        // 待发包只能刷新原来看到的一代。复用同一 tuple 后旧包不能改写新状态。
        generation: Option<u64>,
        segment: Option<tcp::Segment>,
        // 候选转换随待发包保存，重试先确认两侧元组仍可用。
        translation: Option<Datagram>,
    },
    Related {
        key: Key,
        generation: u64,
    },
}

impl Observation {
    pub(super) fn key(self) -> Key {
        match self {
            Self::Data { datagram, .. } => datagram.key,
            Self::Related { key, .. } => key,
        }
    }
}

type Deadline = (Instant, u64);

/// 本次 TX 的候选状态及预留节点。发送失败时直接销毁，排队只保存报文观察。
pub(super) struct Tracked {
    pub(super) observation: Observation,
    hash: u64,
    tcp: Option<tcp::State>,
    restart: bool,
    deadline: Option<VacantEntry<Deadline, ()>>,
    identity: Option<VacantEntry<u64, Key>>,
    tcp_storage: Option<Box<tcp::State>>,
    related: Option<EndpointEdits>,
}

impl Tracked {
    fn new(observation: Observation, hash: u64, tcp: Option<tcp::State>, restart: bool) -> Self {
        Self {
            observation,
            hash,
            tcp,
            restart,
            deadline: None,
            identity: None,
            tcp_storage: None,
            related: None,
        }
    }
}

struct Flow {
    generation: u64,
    expires: Instant,
    scheduled: Instant,
    original_reversed: bool,
    replied: bool,
    tcp: Option<Box<tcp::State>>,
}

/// 连接查询的一次借用投影，不另存连接或 TCP 状态。
#[derive(Clone, Copy)]
struct FlowObservation {
    generation: u64,
    expires: Instant,
    original_reversed: bool,
    replied: bool,
    tcp: Option<tcp::State>,
}

impl FlowObservation {
    fn capture(flow: &Flow) -> Self {
        Self {
            generation: flow.generation,
            expires: flow.expires,
            original_reversed: flow.original_reversed,
            replied: flow.replied,
            tcp: flow.tcp.as_deref().copied(),
        }
    }
}

impl Flow {
    fn phase(&self) -> connections::Phase {
        if let Some(tcp) = &self.tcp {
            tcp.phase()
        } else if self.replied {
            connections::Phase::Established
        } else {
            connections::Phase::Opening
        }
    }

    fn record(&self, key: Key, now: Instant) -> connections::Connection {
        let [first, second] = key.endpoints;
        let ((source, source_port), (destination, destination_port)) = if self.original_reversed {
            (second, first)
        } else {
            (first, second)
        };
        let protocol = match key.protocol {
            Protocol::Tcp => connections::Protocol::Tcp,
            Protocol::Udp => connections::Protocol::Udp,
            Protocol::Echo => connections::Protocol::Echo,
        };
        connections::Connection {
            id: self.generation,
            source: SocketAddr::new(source, source_port),
            destination: SocketAddr::new(destination, destination_port),
            protocol,
            phase: self.phase(),
            idle_ms: (self.expires - now).total_millis(),
        }
    }
}

/// NET guard 下的有界变更历史。首次订阅时预留空间，转发只写状态事件，无逐包分配。
struct Journal {
    events: VecDeque<Change>,
    sequence: u64,
    // 同一位置的等待者共享一次性 Event。置位后移出，后续观察者不会误清其他等待者的信号。
    ready: Option<Arc<Event>>,
}

impl Journal {
    fn new() -> Result<Self, Status> {
        let mut events = VecDeque::new();
        events
            .try_reserve_exact(CHANGE_HISTORY)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            events,
            sequence: 1,
            ready: None,
        })
    }

    fn publish(&mut self, change: Change) {
        if self.events.len() == CHANGE_HISTORY {
            self.events.pop_front();
        }
        self.events.push_back(change);
        self.sequence += 1;
        if let Some(ready) = self.ready.take() {
            let _ = ready.signal(0, signals::USER_0);
        }
    }

    fn read(&self, epoch: u64, cursor: changes::Cursor) -> Result<changes::Batch, Status> {
        let oldest = self.sequence - self.events.len() as u64;
        let reset = cursor.epoch != epoch || !(oldest..=self.sequence).contains(&cursor.sequence);
        let start = if reset {
            self.sequence
        } else {
            cursor.sequence
        };
        let count = (self.sequence - start).min(changes::LIMIT as u64) as usize;
        let mut events = Vec::new();
        events
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        events.extend(
            self.events
                .range((start - oldest) as usize..)
                .take(count)
                .copied(),
        );
        Ok(changes::Batch {
            cursor: changes::Cursor {
                epoch,
                sequence: start + count as u64,
            },
            reset,
            changes: events,
        })
    }
}

#[derive(Clone, Copy)]
pub(in crate::inet) struct FlowHash([u8; 16]);

impl BuildHasher for FlowHash {
    type Hasher = SipHasher13;

    fn build_hasher(&self) -> Self::Hasher {
        SipHasher13::new_with_key(&self.0)
    }
}

/// 网卡队列的只读连接索引。共享的是实际原始连接表，NAT 索引仍归域。
#[derive(Clone)]
pub(in crate::inet) struct ReceiveFlows {
    hash: FlowHash,
    shards: Option<Arc<FlowShards>>,
    udp: [bool; 2],
}

impl ReceiveFlows {
    pub(in crate::inet) fn probe(
        &self,
        packet: device_model::network::ReceivePacket<'_>,
        separate_worker: bool,
    ) -> Option<ReceiveFlow> {
        let (key, hash, bytes) = self.hash.receive(packet)?;
        let observation = match (separate_worker, key.protocol) {
            (true, Protocol::Tcp) => match tcp::Segment::prepare(bytes) {
                Some(segment) => {
                    let [sequence, control] = segment.words();
                    [ReceiveFlow::TCP_PARSED, sequence, control]
                }
                None => [ReceiveFlow::TCP_INVALID, 0, 0],
            },
            (true, Protocol::Udp) if self.udp[usize::from(key.endpoints[0].0.is_ipv6())] => self
                .shards
                .as_ref()
                .and_then(|shards| shards.probe(key, hash))
                .unwrap_or_default(),
            _ => [0; 3],
        };
        Some(ReceiveFlow { hash, observation })
    }
}

/// 绑定于一个原始物理帧的散列及 TCP 头检查结果或 UDP 查找提示。
#[derive(Clone, Copy)]
pub(in crate::inet) struct ReceiveFlow {
    pub(in crate::inet) hash: u64,
    pub(in crate::inet) observation: [u64; 3],
}

impl ReceiveFlow {
    // observation 的含义由同一原始报文的协议决定。UDP 在此位置保存分片版本。
    const TCP_PARSED: u64 = 1;
    const TCP_INVALID: u64 = 2;

    fn prepared_tcp(self) -> Option<Option<tcp::Segment>> {
        match self.observation {
            [Self::TCP_PARSED, sequence, control] => {
                Some(Some(tcp::Segment::from_words([sequence, control])))
            }
            [Self::TCP_INVALID, 0, 0] => Some(None),
            _ => None,
        }
    }
}

/// NET guard 下唯一的连接状态。每流一个定时节点；延长只改真实期限，缩短复用原节点换键。
pub(super) struct Flows {
    tables: Tables,
    identities: FallibleMap<u64, Key>,
    policies: RetainedPolicies,
    epoch: u64,
    generation: u64,
    journal: Option<Journal>,
}

impl Flows {
    pub(super) fn new() -> Self {
        let mut seed = [0; 24];
        native_runtime::system::cprng_draw(&mut seed);
        // 游标实例码与哈希密钥使用独立字节，不通过管理接口公开哈希种子。
        let (key, epoch) = seed.split_at(16);
        let hasher = FlowHash(key.try_into().expect("hash key width"));
        Self {
            tables: Tables::new(hasher),
            identities: FallibleMap::new(),
            policies: RetainedPolicies::new(),
            epoch: u64::from_le_bytes(epoch.try_into().expect("epoch width")).max(1),
            generation: 0,
            journal: None,
        }
    }

    pub(super) fn receive_flows(&self) -> ReceiveFlows {
        ReceiveFlows {
            hash: self.tables.hasher(),
            shards: self.tables.shared().cloned(),
            udp: self.receive_udp(),
        }
    }

    pub(super) fn receive_udp(&self) -> [bool; 2] {
        [false, true].map(|ipv6| self.tables.shared().is_some() && !self.tables.translates(ipv6))
    }

    pub(super) fn observe(
        &self,
        packet: Packet,
        receive: Option<ReceiveFlow>,
        now: Instant,
    ) -> Result<Option<(ConnectionState, Tracked)>, ()> {
        let (datagram, segment) = match packet {
            Packet::Data(datagram) => (datagram, None),
            Packet::Tcp(datagram, segment) => (datagram, Some(segment)),
            Packet::Error(datagram, sequence) => {
                let (datagram, related) = self.quoted_translation(datagram, now)?;
                let Some(flow) = self
                    .tables
                    .flow(datagram.key)
                    .filter(|flow| flow.expires > now)
                else {
                    return Ok(None);
                };
                let matches = if let Some(sequence) = sequence {
                    flow.tcp
                        .as_ref()
                        .is_some_and(|tcp| tcp.contains(usize::from(datagram.reversed), sequence))
                } else {
                    flow.replied || flow.original_reversed == datagram.reversed
                };
                if !matches && related.is_some() {
                    return Err(());
                }
                return Ok(matches.then(|| {
                    let observation = Observation::Related {
                        key: datagram.key,
                        generation: flow.generation,
                    };
                    let mut tracked = Tracked::new(
                        observation,
                        self.tables.hash_key(observation.key()),
                        None,
                        false,
                    );
                    tracked.related = related;
                    (ConnectionState::Related, tracked)
                }));
            }
        };
        if !trackable_addresses(datagram.key.endpoints[0].0, datagram.key.endpoints[1].0) {
            return Ok(None);
        }
        let receive =
            receive.filter(|_| matches!(datagram.key.protocol, Protocol::Tcp | Protocol::Udp));
        debug_assert!(
            receive.is_none_or(|receive| receive.hash == self.tables.hash_key(datagram.key))
        );
        let hash = receive.map_or_else(
            || self.tables.hash_key(datagram.key),
            |receive| receive.hash,
        );
        // 线上别名及其发布由域协调，存在同族转换时必须先按线上键查实际别名。
        if datagram.key.protocol == Protocol::Udp
            && !self
                .tables
                .translates(datagram.key.endpoints[0].0.is_ipv6())
            && let Some(flow) = receive
                .and_then(|receive| self.tables.validate_probe(hash, receive.observation, now))
        {
            return Ok(Self::track_data(datagram, hash, flow, None, now));
        }
        let (datagram, hash, lookup) = self.original_datagram(datagram, hash, now)?;
        let observation = Self::track_data(datagram, hash, lookup.flow, segment, now);
        if lookup.outgoing.is_some() {
            let Some((_, tracked)) = &observation else {
                return Err(());
            };
            // 反方向上的新 SYN 不能把已绑定的转换改成另一首发方向。
            if tracked.restart
                && datagram.reversed != lookup.flow.expect("indexed flow").original_reversed
            {
                return Err(());
            }
        }
        Ok(observation)
    }

    fn track_data(
        datagram: Datagram,
        hash: u64,
        flow: Option<FlowObservation>,
        segment: Option<tcp::Segment>,
        now: Instant,
    ) -> Option<(ConnectionState, Tracked)> {
        let flow = flow.filter(|flow| flow.expires > now);
        let direction = usize::from(datagram.reversed);
        let (tcp, restart) = if let Some(segment) = segment {
            match flow.and_then(|flow| flow.tcp) {
                Some(state) if !state.may_restart(segment, direction) => {
                    (Some(state.advance(segment, direction)?), false)
                }
                _ => (Some(tcp::State::open(segment, direction)?), flow.is_some()),
            }
        } else {
            (None, false)
        };
        let state = match flow {
            Some(flow)
                if !restart && (flow.replied || flow.original_reversed != datagram.reversed) =>
            {
                ConnectionState::Established
            }
            None if datagram.key.protocol == Protocol::Echo && datagram.reversed => return None,
            _ => ConnectionState::New,
        };
        Some((
            state,
            Tracked::new(
                Observation::Data {
                    datagram,
                    segment,
                    generation: flow.map(|flow| flow.generation),
                    translation: None,
                },
                hash,
                tcp,
                restart,
            ),
        ))
    }

    // 发送前预留空间。容量不足时丢弃，成功提交前不发布表项。
    pub(super) fn reserve(&mut self, mut tracked: Tracked, now: Instant) -> Option<Tracked> {
        let Observation::Data {
            datagram,
            generation,
            translation,
            ..
        } = tracked.observation
        else {
            return Some(tracked);
        };
        if self
            .tables
            .hashed_incoming(datagram.key, tracked.hash)
            .is_some()
        {
            return None;
        }
        let current = if generation.is_none() || tracked.restart {
            self.tables.hashed_flow(datagram.key, tracked.hash)
        } else {
            None
        };
        if generation.is_none() || tracked.restart {
            if current.is_some_and(|flow| flow.expires > now) && !tracked.restart {
                if self.tables.hashed_outgoing(datagram.key, tracked.hash) != translation {
                    return None;
                }
            } else if let Some(wire) = translation {
                if !self.wire_available(datagram.key, wire.key) {
                    return None;
                }
                self.tables.reserve_translation(datagram.key, wire.key)?;
            }
        }
        if generation.is_none() && current.is_none() {
            if self.identities.len() >= CAPACITY {
                return None;
            }
            self.tables.reserve_flow(tracked.hash)?;
            tracked.deadline = Some(FallibleMap::try_prepare((now, 0), ()).ok()?);
            tracked.identity = Some(FallibleMap::try_prepare(0, datagram.key).ok()?);
            if let Some(tcp) = tracked.tcp {
                tracked.tcp_storage = Some(Box::try_new(tcp).ok()?);
            }
        }
        Some(tracked)
    }

    pub(super) fn retry(&mut self, observation: Observation, now: Instant) -> Option<Tracked> {
        let hash = self.tables.hash_key(observation.key());
        let tracked = match observation {
            Observation::Data {
                datagram,
                generation,
                segment: Some(segment),
                translation,
            } => {
                let flow = self.tables.hashed_flow(datagram.key, hash);
                let (_, mut tracked) = Self::track_data(datagram, hash, flow, Some(segment), now)?;
                let Observation::Data {
                    generation: current,
                    translation: ref mut pending,
                    ..
                } = tracked.observation
                else {
                    unreachable!("TCP tracking produces a data observation");
                };
                if generation != current && (generation.is_some() || tracked.restart) {
                    return None;
                }
                if generation.is_none()
                    && current.is_some()
                    && self.tables.hashed_outgoing(datagram.key, hash) != translation
                {
                    return None;
                }
                *pending = translation;
                tracked
            }
            _ => {
                let generation = match observation {
                    Observation::Data { generation, .. } => generation,
                    Observation::Related { generation, .. } => Some(generation),
                };
                // 到期或被清除的流不能授权已经等待出口的回包或关联错误。
                if generation.is_some_and(|id| {
                    self.tables
                        .hashed_flow(observation.key(), hash)
                        .is_none_or(|flow| flow.generation != id || flow.expires <= now)
                }) {
                    return None;
                }
                Tracked::new(observation, hash, None, false)
            }
        };
        self.reserve(tracked, now)
    }

    pub(super) fn confirm(&mut self, tracked: Tracked, now: Instant) {
        let Observation::Data {
            datagram,
            generation,
            segment,
            translation,
        } = tracked.observation
        else {
            return;
        };
        let key = datagram.key;
        let idle = |replied| {
            tracked.tcp.map_or_else(
                || {
                    if replied && key.protocol == Protocol::Udp {
                        UDP_REPLIED
                    } else {
                        SHORT_IDLE
                    }
                },
                tcp::State::idle,
            )
        };
        let edited = if tracked.deadline.is_some() {
            None
        } else {
            self.tables.edit_flow(key, tracked.hash, |slot, flow| {
                let previous_id = flow.generation;
                // 没有回包的原方向 SYN 重传不延长半开期限。
                if generation == Some(flow.generation)
                    && flow.original_reversed == datagram.reversed
                    && !tracked.restart
                    && segment.is_some_and(|segment| {
                        flow.tcp
                            .as_ref()
                            .is_some_and(|tcp| tcp.awaits_reply(segment))
                    })
                {
                    return None;
                }
                let previous_phase = self.journal.as_ref().map(|_| flow.phase());
                if flow.expires > now
                    && generation.is_none_or(|generation| generation == flow.generation)
                {
                    if tracked.restart {
                        self.generation += 1;
                        flow.generation = self.generation;
                        flow.original_reversed = datagram.reversed;
                        flow.replied = false;
                    } else {
                        flow.replied |= flow.original_reversed != datagram.reversed;
                    }
                } else if flow.expires <= now && generation.is_none() {
                    self.generation += 1;
                    flow.generation = self.generation;
                    flow.original_reversed = datagram.reversed;
                    flow.replied = false;
                } else {
                    return None;
                }
                if let Some(tcp) = tracked.tcp {
                    **flow.tcp.as_mut().expect("TCP flow storage") = tcp;
                }
                flow.expires = now + idle(flow.replied);
                let reschedule = if flow.expires < flow.scheduled || flow.generation != previous_id
                {
                    let previous = (flow.scheduled, previous_id);
                    flow.scheduled = flow.expires;
                    Some((previous, (flow.scheduled, flow.generation)))
                } else {
                    None
                };
                let replaced = flow.generation != previous_id;
                let change = self.journal.as_ref().and_then(|_| {
                    if replaced {
                        Some(Change::Added(flow.record(key, now)))
                    } else if previous_phase != Some(flow.phase()) {
                        Some(Change::Updated(flow.record(key, now)))
                    } else {
                        None
                    }
                });
                Some((slot, previous_id, flow.generation, reschedule, change))
            })
        };
        if let Some(edited) = edited {
            let Some((slot, previous_id, generation, reschedule, change)) = edited else {
                return;
            };
            let replaced = generation != previous_id;
            if replaced {
                let mut identity = self
                    .identities
                    .take_entry(&previous_id)
                    .expect("indexed flow");
                identity.set_key(generation);
                self.identities.commit_vacant(identity);
            }
            if let Some((previous, deadline)) = reschedule {
                self.tables.reschedule(slot, previous, deadline);
            }
            if replaced {
                self.tables.replace_translation(key, translation);
                self.retire_policy(previous_id);
            }
            if let Some((journal, change)) = self.journal.as_mut().zip(change) {
                if replaced {
                    journal.publish(Change::Removed(previous_id));
                }
                journal.publish(change);
            }
        } else if let Some(mut deadline) = tracked.deadline {
            self.generation += 1;
            let expires = now + idle(false);
            let flow = Flow {
                generation: self.generation,
                expires,
                scheduled: expires,
                original_reversed: datagram.reversed,
                replied: false,
                tcp: tracked.tcp_storage,
            };
            let added = self.journal.as_ref().map(|_| flow.record(key, now));
            if let Some(wire) = translation {
                self.tables.install_translation(key, wire);
            }
            let slot = self.tables.insert_flow(key, tracked.hash, flow);
            let mut identity = tracked.identity.expect("reserved flow identity");
            identity.set_key(self.generation);
            self.identities.commit_vacant(identity);
            deadline.set_key((expires, self.generation));
            self.tables.schedule(slot, deadline);
            if let Some((journal, added)) = self.journal.as_mut().zip(added) {
                journal.publish(Change::Added(added));
            }
        }
    }

    pub(super) fn page(
        &self,
        cursor: connections::Cursor,
        now: Instant,
    ) -> Result<connections::Page, native_abi::Status> {
        if cursor.epoch != 0 && cursor.epoch != self.epoch {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let mut page = connections::Page {
            epoch: self.epoch,
            rows: alloc::vec::Vec::new(),
            next: None,
        };
        page.rows
            .try_reserve_exact(self.identities.len().min(connections::PAGE_LIMIT))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut positions = self.identities.iter_after(&cursor.after);
        let mut last = cursor.after;
        // 大量到期项尚在分批退休时也限制锁内工作；空页仍可推进游标。
        for (id, key) in positions.by_ref().take(connections::PAGE_LIMIT * 4) {
            last = *id;
            let row = self
                .tables
                .with_flow(*key, |flow| {
                    (flow.expires > now).then(|| flow.record(*key, now))
                })
                .expect("indexed flow");
            let Some(row) = row else {
                continue;
            };
            page.rows.push(row);
            if page.rows.len() == connections::PAGE_LIMIT {
                break;
            }
        }
        page.next = positions.next().map(|_| last);
        Ok(page)
    }

    pub(super) fn changes(&mut self, cursor: changes::Cursor) -> Result<changes::Batch, Status> {
        if self.journal.is_none() {
            self.journal = Some(Journal::new()?);
        }
        self.journal
            .as_ref()
            .expect("prepared history")
            .read(self.epoch, cursor)
    }

    pub(super) fn arm_watch(
        &mut self,
        cursor: changes::Cursor,
        create: impl FnOnce() -> Result<Arc<Event>, Status>,
    ) -> Result<Option<Arc<Event>>, Status> {
        let Some(journal) = &mut self.journal else {
            return Ok(None);
        };
        if cursor.epoch != self.epoch || cursor.sequence != journal.sequence {
            return Ok(None);
        }
        if journal.ready.is_none() {
            journal.ready = Some(create()?);
        }
        Ok(journal.ready.clone())
    }

    pub(super) fn due(&self, now: Instant) -> bool {
        self.next_deadline().is_some_and(|at| at <= now)
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.tables.next_deadline()
    }

    pub(super) fn clear(
        &mut self,
        identity: connections::Identity,
        now: Instant,
    ) -> Result<Key, native_abi::Status> {
        if identity.epoch != self.epoch {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        let key = *self
            .identities
            .get(&identity.id)
            .ok_or(native_abi::ERR_NOT_FOUND)?;
        let (expires, scheduled) = self
            .tables
            .with_flow(key, |flow| (flow.expires, flow.scheduled))
            .expect("indexed flow");
        if expires <= now {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        self.remove_record(key, (scheduled, identity.id));
        Ok(key)
    }

    fn remove_record(&mut self, key: Key, deadline: Deadline) {
        self.tables.unschedule(key, deadline);
        self.identities.remove(&deadline.1);
        self.retire_policy(deadline.1);
        self.tables.remove_flow(key);
        if self.identities.is_empty() {
            self.tables.reset_empty();
        }
        if let Some(journal) = &mut self.journal {
            journal.publish(Change::Removed(deadline.1));
        }
    }

    /// 旧 TCP 已关闭，新 SYN 交回本机时释放原转发身份和转换。
    pub(super) fn retire_local_restart(&mut self, tracked: &Tracked) {
        if tracked.restart {
            let key = tracked.observation.key();
            let deadline = self
                .tables
                .with_flow(key, |flow| (flow.scheduled, flow.generation))
                .expect("indexed flow");
            self.remove_record(key, deadline);
        }
    }

    pub(super) fn retire(&mut self, now: Instant, work_until: u64) -> bool {
        let mut remaining = RETIRE_BATCH;
        while remaining != 0 {
            let Some(slot) = self.tables.next_ready_shard(now) else {
                return false;
            };
            while remaining != 0 {
                let Some(deadline @ (_, id)) = self.tables.due_in_shard(slot, now) else {
                    break;
                };
                remaining -= 1;
                let key = *self.identities.get(&id).expect("indexed flow");
                let hash = self.tables.hash_key(key);
                let refreshed = self
                    .tables
                    .edit_flow(key, hash, |owner, flow| {
                        debug_assert_eq!(slot, owner);
                        if flow.expires <= now {
                            return None;
                        }
                        flow.scheduled = flow.expires;
                        Some((flow.scheduled, id))
                    })
                    .expect("scheduled flow");
                if let Some(next) = refreshed {
                    self.tables.reschedule(slot, deadline, next);
                } else {
                    self.remove_record(key, deadline);
                }
                if remaining.is_multiple_of(RETIRE_CLOCK_INTERVAL)
                    && work_until != u64::MAX
                    && crate::time::get_time_us() >= work_until
                {
                    return self.due(now);
                }
            }
        }
        self.due(now)
    }
}
