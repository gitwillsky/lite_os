//! 双栈传输共用双向连接跟踪和定时清理。正反方向共用一个表项，只有实际提交转发才确认或刷新状态。

mod tcp;

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    hash::BuildHasher,
    net::{IpAddr, SocketAddr},
};
use fallible_tree::{FallibleMap, VacantEntry};
use hashbrown::HashMap;
use native_runtime::{
    Event,
    native_abi::{self, Status, signals},
};
use packet_wire::{
    icmpv4::{self, Icmpv4Packet},
    icmpv6::{self, Icmpv6Packet},
    ipv4::{Ipv4Packet, Ipv4Quote},
    ipv6::{Ipv6Datagram, Ipv6Packet, Ipv6Quote},
    tcp::TcpPacket,
    udp::UdpPacket,
};
use router_protocol::connections::changes::{self, Change};
use router_protocol::{connections, firewall::ConnectionState};
use siphasher::sip::SipHasher13;
use smoltcp::time::{Duration, Instant};

const CAPACITY: usize = 1_000_000;
const RETIRE_BATCH: usize = 64;
const SHORT_IDLE: Duration = Duration::from_millis(30_000);
const UDP_REPLIED: Duration = Duration::from_millis(120_000);
const CHANGE_HISTORY: usize = 1024;

type Endpoint = (IpAddr, u16);

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum Protocol {
    Tcp,
    Udp,
    Echo,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct Key {
    endpoints: [Endpoint; 2],
    protocol: Protocol,
}

#[derive(Clone, Copy)]
pub(super) struct Datagram {
    key: Key,
    reversed: bool,
}

impl Datagram {
    fn ports(protocol: Protocol, source: Endpoint, destination: Endpoint) -> Option<Self> {
        if source == destination || source.1 == 0 || destination.1 == 0 {
            return None;
        }
        let reversed = source > destination;
        let endpoints = if reversed {
            [destination, source]
        } else {
            [source, destination]
        };
        Some(Self {
            key: Key {
                endpoints,
                protocol,
            },
            reversed,
        })
    }

    fn echo(source: IpAddr, destination: IpAddr, identifier: u16, reply: bool) -> Option<Self> {
        if source == destination {
            return None;
        }
        // Echo 发起者由消息类型确定。两端并发使用同一个 ID 时仍是两条流。
        let (origin, peer) = if reply {
            (destination, source)
        } else {
            (source, destination)
        };
        Some(Self {
            key: Key {
                endpoints: [(origin, identifier), (peer, 0)],
                protocol: Protocol::Echo,
            },
            reversed: reply,
        })
    }

    fn quoted(source: IpAddr, destination: IpAddr, protocol: u8, bytes: &[u8]) -> Option<Packet> {
        if !trackable_addresses(source, destination) {
            return None;
        }
        match (source.is_ipv6(), protocol) {
            (_, 6) => {
                let [a, b, c, d, e, f, g, h] = *bytes.first_chunk::<8>()?;
                let datagram = Self::ports(
                    Protocol::Tcp,
                    (source, u16::from_be_bytes([a, b])),
                    (destination, u16::from_be_bytes([c, d])),
                )?;
                Some(Packet::Error(
                    datagram,
                    Some(u32::from_be_bytes([e, f, g, h])),
                ))
            }
            (_, 17) => {
                let udp = UdpPacket::parse_quote(bytes, source.is_ipv6()).ok()?;
                Self::ports(
                    Protocol::Udp,
                    (source, udp.source_port()),
                    (destination, udp.destination_port()),
                )
                .map(|datagram| Packet::Error(datagram, None))
            }
            (false, 1) => {
                let icmpv4::Message::Echo {
                    reply, identifier, ..
                } = Icmpv4Packet::parse_header(bytes).ok()?.message()
                else {
                    return None;
                };
                Self::echo(source, destination, identifier, reply)
                    .map(|datagram| Packet::Error(datagram, None))
            }
            (true, 58) => {
                let icmpv6::Message::Echo {
                    reply, identifier, ..
                } = Icmpv6Packet::parse_header(bytes).ok()?.message()
                else {
                    return None;
                };
                Self::echo(source, destination, identifier, reply)
                    .map(|datagram| Packet::Error(datagram, None))
            }
            _ => None,
        }
    }
}

fn trackable_addresses(source: IpAddr, destination: IpAddr) -> bool {
    [source, destination]
        .into_iter()
        .all(|address| !matches!(address, IpAddr::V6(address) if address.is_unicast_link_local()))
}

pub(super) enum Packet {
    Data(Datagram),
    Tcp(Datagram, tcp::Segment),
    Error(Datagram, Option<u32>),
}

impl Packet {
    pub(super) fn ipv4(ip: Ipv4Packet<'_>, checksum_valid: bool) -> Option<Self> {
        if ip.fragment_offset() != 0 || ip.more_fragments() {
            return None;
        }
        let source = ip.source().into();
        let destination = ip.destination().into();
        match ip.protocol() {
            6 => Self::tcp(source, destination, ip.payload(), checksum_valid),
            17 => Self::udp(source, destination, ip.payload(), checksum_valid),
            1 => {
                let icmp = if checksum_valid {
                    Icmpv4Packet::parse_header(ip.payload())
                } else {
                    Icmpv4Packet::parse(ip.payload())
                }
                .ok()?;
                match icmp.message() {
                    icmpv4::Message::Echo {
                        reply, identifier, ..
                    } => Datagram::echo(source, destination, identifier, reply).map(Self::Data),
                    icmpv4::Message::Error { body, .. }
                        if matches!(
                            (icmp.message_type(), icmp.code()),
                            (3, 0..=15) | (11, 0..=1) | (12, 0..=2)
                        ) =>
                    {
                        let quote = Ipv4Quote::parse(body.quoted_bytes().ok()?).ok()?;
                        // 外层目的必须是被引用方向的发送端，不能把已知 tuple 当作跨主机放行凭据。
                        if quote.source() != ip.destination() {
                            return None;
                        }
                        Datagram::quoted(
                            quote.source().into(),
                            quote.destination().into(),
                            quote.protocol(),
                            quote.upper_layer()?,
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(super) fn ipv6(ip: Ipv6Datagram<'_>, checksum_valid: bool) -> Option<Self> {
        let source = ip.source().into();
        let destination = ip.destination().into();
        if !trackable_addresses(source, destination) {
            return None;
        }
        let packet = Ipv6Packet::parse(ip.bytes()).ok()?;
        let bytes = packet.upper_layer()?;
        match packet.upper_protocol() {
            6 => Self::tcp(source, destination, bytes, checksum_valid),
            17 => Self::udp(source, destination, bytes, checksum_valid),
            58 => {
                let icmp = if checksum_valid {
                    Icmpv6Packet::parse_header(bytes)
                } else {
                    Icmpv6Packet::parse(bytes, ip.source(), ip.destination())
                }
                .ok()?;
                match icmp.message() {
                    icmpv6::Message::Echo {
                        reply, identifier, ..
                    } => Datagram::echo(source, destination, identifier, reply).map(Self::Data),
                    icmpv6::Message::Error { body, .. }
                        if matches!(
                            (icmp.message_type(), icmp.code()),
                            (1, 0..=6) | (2, 0) | (3, 0..=1) | (4, 0..=2)
                        ) =>
                    {
                        let quote = Ipv6Quote::parse(body.quoted_bytes().ok()?).ok()?;
                        if quote.source() != ip.destination() {
                            return None;
                        }
                        Datagram::quoted(
                            quote.source().into(),
                            quote.destination().into(),
                            quote.upper_protocol(),
                            quote.upper_layer()?,
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn tcp(
        source: IpAddr,
        destination: IpAddr,
        bytes: &[u8],
        checksum_valid: bool,
    ) -> Option<Self> {
        let packet = if checksum_valid {
            TcpPacket::parse_header(bytes)
        } else {
            TcpPacket::parse(bytes, source, destination)
        }
        .ok()?;
        let header = packet.header();
        let datagram = Datagram::ports(
            Protocol::Tcp,
            (source, header.source_port()),
            (destination, header.destination_port()),
        )?;
        Some(Self::Tcp(datagram, tcp::Segment::capture(packet)?))
    }

    fn udp(
        source: IpAddr,
        destination: IpAddr,
        bytes: &[u8],
        checksum_valid: bool,
    ) -> Option<Self> {
        let udp = if checksum_valid {
            UdpPacket::parse_header(bytes, source.is_ipv6())
        } else {
            UdpPacket::parse(bytes, source, destination)
        }
        .ok()?;
        Datagram::ports(
            Protocol::Udp,
            (source, udp.source_port()),
            (destination, udp.destination_port()),
        )
        .map(Self::Data)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Observation {
    Data {
        datagram: Datagram,
        // 待发包只能刷新原来看到的一代。复用同一 tuple 后旧包不能改写新状态。
        generation: Option<u64>,
        segment: Option<tcp::Segment>,
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
    tcp: Option<tcp::State>,
    restart: bool,
    deadline: Option<VacantEntry<Deadline, ()>>,
    identity: Option<VacantEntry<u64, Key>>,
    tcp_storage: Option<Box<tcp::State>>,
}

impl Tracked {
    fn new(observation: Observation, tcp: Option<tcp::State>, restart: bool) -> Self {
        Self {
            observation,
            tcp,
            restart,
            deadline: None,
            identity: None,
            tcp_storage: None,
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
struct FlowHash([u8; 16]);
impl BuildHasher for FlowHash {
    type Hasher = SipHasher13;

    fn build_hasher(&self) -> Self::Hasher {
        SipHasher13::new_with_key(&self.0)
    }
}

/// NET guard 下唯一的连接状态。每流一个定时节点；延长只改真实期限，缩短复用原节点换键。
pub(super) struct Flows {
    entries: HashMap<Key, Flow, FlowHash>,
    deadlines: FallibleMap<Deadline, ()>,
    identities: FallibleMap<u64, Key>,
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
        Self {
            entries: HashMap::with_hasher(FlowHash(key.try_into().expect("hash key width"))),
            deadlines: FallibleMap::new(),
            identities: FallibleMap::new(),
            epoch: u64::from_le_bytes(epoch.try_into().expect("epoch width")).max(1),
            generation: 0,
            journal: None,
        }
    }

    pub(super) fn observe(
        &self,
        packet: Packet,
        now: Instant,
    ) -> Option<(ConnectionState, Tracked)> {
        match packet {
            Packet::Data(datagram) => self.track_data(datagram, None, now),
            Packet::Tcp(datagram, segment) => self.track_data(datagram, Some(segment), now),
            Packet::Error(datagram, sequence) => {
                let flow = self
                    .entries
                    .get(&datagram.key)
                    .filter(|flow| flow.expires > now)?;
                let matches = if let Some(sequence) = sequence {
                    flow.tcp
                        .as_ref()
                        .is_some_and(|tcp| tcp.contains(usize::from(datagram.reversed), sequence))
                } else {
                    flow.replied || flow.original_reversed == datagram.reversed
                };
                matches.then(|| {
                    let observation = Observation::Related {
                        key: datagram.key,
                        generation: flow.generation,
                    };
                    (
                        ConnectionState::Related,
                        Tracked::new(observation, None, false),
                    )
                })
            }
        }
    }

    fn track_data(
        &self,
        datagram: Datagram,
        segment: Option<tcp::Segment>,
        now: Instant,
    ) -> Option<(ConnectionState, Tracked)> {
        let flow = self
            .entries
            .get(&datagram.key)
            .filter(|flow| flow.expires > now);
        let direction = usize::from(datagram.reversed);
        let (tcp, restart) = if let Some(segment) = segment {
            match flow.and_then(|flow| flow.tcp.as_deref()) {
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
                },
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
            ..
        } = tracked.observation
        else {
            return Some(tracked);
        };
        if generation.is_none() && !self.entries.contains_key(&datagram.key) {
            if self.entries.len() >= CAPACITY {
                return None;
            }
            self.entries.try_reserve(1).ok()?;
            tracked.deadline = Some(FallibleMap::try_prepare((now, 0), ()).ok()?);
            tracked.identity = Some(FallibleMap::try_prepare(0, datagram.key).ok()?);
            if let Some(tcp) = tracked.tcp {
                tracked.tcp_storage = Some(Box::try_new(tcp).ok()?);
            }
        }
        Some(tracked)
    }

    pub(super) fn retry(&mut self, observation: Observation, now: Instant) -> Option<Tracked> {
        let tracked = match observation {
            Observation::Data {
                datagram,
                generation,
                segment: Some(segment),
            } => {
                let (_, tracked) = self.track_data(datagram, Some(segment), now)?;
                let Observation::Data {
                    generation: current,
                    ..
                } = tracked.observation
                else {
                    unreachable!("TCP tracking produces a data observation");
                };
                if generation != current && (generation.is_some() || tracked.restart) {
                    return None;
                }
                tracked
            }
            _ => {
                let generation = match observation {
                    Observation::Data { generation, .. } => generation,
                    Observation::Related { generation, .. } => Some(generation),
                };
                // 到期或被清除的流不能授权已经等待出口的回包或关联错误。
                if generation.is_some_and(|id| {
                    self.entries
                        .get(&observation.key())
                        .is_none_or(|flow| flow.generation != id || flow.expires <= now)
                }) {
                    return None;
                }
                Tracked::new(observation, None, false)
            }
        };
        self.reserve(tracked, now)
    }

    pub(super) fn confirm(&mut self, tracked: Tracked, now: Instant) {
        let Observation::Data {
            datagram,
            generation,
            segment,
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
        if let Some(flow) = self.entries.get_mut(&key) {
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
                return;
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
                return;
            }
            if let Some(tcp) = tracked.tcp {
                **flow.tcp.as_mut().expect("TCP flow storage") = tcp;
            }
            flow.expires = now + idle(flow.replied);
            if flow.generation != previous_id {
                let mut identity = self
                    .identities
                    .take_entry(&previous_id)
                    .expect("indexed flow");
                identity.set_key(flow.generation);
                self.identities.commit_vacant(identity);
            }
            if flow.expires < flow.scheduled || flow.generation != previous_id {
                let mut deadline = self
                    .deadlines
                    .take_entry(&(flow.scheduled, previous_id))
                    .expect("scheduled flow");
                flow.scheduled = flow.expires;
                deadline.set_key((flow.scheduled, flow.generation));
                self.deadlines.commit_vacant(deadline);
            }
            if let Some(journal) = &mut self.journal {
                if flow.generation != previous_id {
                    journal.publish(Change::Removed(previous_id));
                    journal.publish(Change::Added(flow.record(key, now)));
                } else if previous_phase != Some(flow.phase()) {
                    journal.publish(Change::Updated(flow.record(key, now)));
                }
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
            self.entries.insert(key, flow);
            let mut identity = tracked.identity.expect("reserved flow identity");
            identity.set_key(self.generation);
            self.identities.commit_vacant(identity);
            deadline.set_key((expires, self.generation));
            self.deadlines.commit_vacant(deadline);
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
            let flow = &self.entries[key];
            if flow.expires <= now {
                continue;
            }
            page.rows.push(flow.record(*key, now));
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
        self.deadlines
            .first_key_value()
            .is_some_and(|((deadline, _), ())| *deadline <= now)
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
        let flow = &self.entries[&key];
        if flow.expires <= now {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        self.remove_record(key, (flow.scheduled, identity.id));
        Ok(key)
    }

    fn remove_record(&mut self, key: Key, deadline: Deadline) {
        self.deadlines.remove(&deadline);
        self.identities.remove(&deadline.1);
        self.entries.remove(&key);
        if self.entries.is_empty() {
            self.entries = HashMap::with_hasher(*self.entries.hasher());
        }
        if let Some(journal) = &mut self.journal {
            journal.publish(Change::Removed(deadline.1));
        }
    }

    pub(super) fn retire(&mut self, now: Instant) -> bool {
        for _ in 0..RETIRE_BATCH {
            let Some((&(deadline, id), ())) = self.deadlines.first_key_value() else {
                return false;
            };
            if deadline > now {
                return false;
            }
            let key = *self.identities.get(&id).expect("indexed flow");
            let flow = self.entries.get_mut(&key).expect("scheduled flow");
            if flow.expires > now {
                let mut node = self
                    .deadlines
                    .take_entry(&(deadline, id))
                    .expect("due flow deadline");
                flow.scheduled = flow.expires;
                node.set_key((flow.scheduled, id));
                self.deadlines.commit_vacant(node);
            } else {
                self.remove_record(key, (deadline, id));
                if self.entries.is_empty() {
                    return false;
                }
            }
        }
        self.due(now)
    }
}
