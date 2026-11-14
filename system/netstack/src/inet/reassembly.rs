//! 双栈分片重组。本机交付和有状态转发共用区间检查，各自持有独立预算。
mod packet;
pub(super) mod reply;
use alloc::vec::Vec;
use core::net::IpAddr;
use fallible_tree::{FallibleMap, VacantEntry};
use ::tcp::time::Instant;

// OWNER: 仅实际分片才占容量；限制敌对乱序流量的堆占用和每轮回收工作，不预分配大缓冲池。
const DATAGRAMS: usize = 128;
const BYTES: usize = 4 * 1024 * 1024;
const LIFETIME_MS: i64 = 60_000;
const RETIRE_BUDGET: usize = 16;
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    source: IpAddr,
    destination: IpAddr,
    id: u32,
    protocol: u8,
    zone: u32,
}
struct Range {
    end: u16,
    more: bool,
    // 转发须保留每片线上格式；本机只保留连续载荷。
    wire: Vec<u8>,
}
struct First {
    // 原始首片用于超时引用；不含 Ethernet，至少保留全部不可分片头，普通引用最多 1232 字节。
    quote: Vec<u8>,
    header: usize,
    next_field: Option<usize>,
}
enum InsertError {
    Discard,
    Oversize,
}
enum Payload {
    Local(Vec<u8>),
    // 转换只允许原片全部来自同一个合格的物理入口。跨口重组仍可交本机。
    Wire {
        capacity: usize,
        ingress: Option<u32>,
    },
}
impl Payload {
    fn charge(&self) -> usize {
        match self {
            Self::Local(bytes) => bytes.capacity(),
            Self::Wire { capacity, .. } => *capacity,
        }
    }
}
struct Datagram {
    ranges: FallibleMap<u16, Range>,
    payload: Payload,
    first: Option<First>,
    last: Option<usize>,
    received: usize,
    end: usize,
    ecn: u8,
    origin: u32,
    expires: Instant,
}
impl Datagram {
    fn node_charge() -> usize {
        VacantEntry::<Key, Self>::allocation_size()
            + VacantEntry::<(Instant, Key), ()>::allocation_size()
    }

    fn charge(&self) -> usize {
        Self::node_charge()
            + self.ranges.len() * VacantEntry::<u16, Range>::allocation_size()
            + self.payload.charge()
            + self
                .first
                .as_ref()
                .map_or(0, |first| first.quote.capacity())
    }
    fn new(origin: u32, now: Instant, retain_wire: bool) -> Self {
        Self {
            ranges: FallibleMap::new(),
            payload: if retain_wire {
                Payload::Wire {
                    capacity: 0,
                    ingress: Some(origin),
                }
            } else {
                Payload::Local(Vec::new())
            },
            first: None,
            last: None,
            received: 0,
            end: 0,
            ecn: 0,
            origin,
            expires: now + ::tcp::time::Duration::from_millis(LIFETIME_MS as u64),
        }
    }
    /// 区间 AVL 只检查相邻范围；乱序/重叠不触发线性移动或重复写载荷。
    fn insert(
        &mut self,
        fragment: &packet::Fragment<'_>,
        available: usize,
        (retain_wire, transit): (bool, bool),
    ) -> Result<(), InsertError> {
        let start = fragment.offset;
        let end = start + fragment.payload.len();
        if fragment.payload.is_empty()
            || self
                .last
                .is_some_and(|last| end > last || fragment.more && end == last)
            || !fragment.more && (self.last.is_some_and(|last| end != last) || self.end > end)
        {
            return Err(InsertError::Discard);
        }
        if let Some(previous) = self.ranges.get(&(start as u16)) {
            return if usize::from(previous.end) == end
                && previous.more == fragment.more
                && match &self.payload {
                    Payload::Local(payload) => payload[start..end] == *fragment.payload,
                    Payload::Wire { .. } if transit => previous.wire == fragment.bytes,
                    Payload::Wire { .. } => {
                        previous.wire[previous.wire.len() - fragment.payload.len()..]
                            == *fragment.payload
                    }
                } {
                Ok(())
            } else {
                Err(InsertError::Discard)
            };
        }
        if self
            .ranges
            .predecessor(&(start as u16))
            .is_some_and(|(_, range)| usize::from(range.end) > start)
            || self
                .ranges
                .successor(&(start as u16))
                .is_some_and(|(&next, _)| usize::from(next) < end)
        {
            return Err(InsertError::Discard);
        }
        let first_length = if start == 0 {
            match self.payload {
                Payload::Local(_) => fragment.bytes.len().min(fragment.data.max(1232)),
                Payload::Wire { .. } => 0,
            }
        } else {
            0
        };
        // 倍增而非逐片精确扩容；否则八字节小片会造成二次方的载荷搬移。
        let capacity = match &self.payload {
            Payload::Local(payload) if end > payload.capacity() => {
                end.max(payload.capacity().saturating_mul(2)).min(65535)
            }
            Payload::Local(payload) => payload.capacity(),
            Payload::Wire { capacity, .. } => capacity + fragment.bytes.len(),
        };
        let required = capacity
            + first_length.max(self.first.as_ref().map_or(0, |f| f.quote.capacity()))
            + Self::node_charge()
            + (self.ranges.len() + 1) * VacantEntry::<u16, Range>::allocation_size();
        if required > available {
            return Err(InsertError::Discard);
        }
        let mut wire = Vec::new();
        if matches!(self.payload, Payload::Wire { .. }) {
            wire.try_reserve_exact(fragment.bytes.len())
                .map_err(|_| InsertError::Discard)?;
            wire.extend_from_slice(fragment.bytes);
        }
        let wire_capacity = wire.capacity();
        let range = FallibleMap::try_prepare(
            start as u16,
            Range {
                end: end as u16,
                more: fragment.more,
                wire,
            },
        )
        .map_err(|_| InsertError::Discard)?;
        if start == 0 {
            let mut quote = Vec::new();
            quote
                .try_reserve_exact(first_length)
                .map_err(|_| InsertError::Discard)?;
            quote.extend_from_slice(&fragment.bytes[..first_length]);
            self.first = Some(First {
                quote,
                header: fragment.header,
                next_field: fragment.next_field,
            });
            self.origin = fragment.interface;
        }
        // 先完成全部 fallible 准备；失败由外层整体退休此包，不能留下不可计量的半个区间。
        match &mut self.payload {
            Payload::Local(payload) => {
                if end > payload.len() {
                    payload
                        .try_reserve_exact(capacity - payload.len())
                        .map_err(|_| InsertError::Discard)?;
                    payload.resize(end, 0);
                }
                payload[start..end].copy_from_slice(fragment.payload);
            }
            Payload::Wire { capacity, ingress } => {
                *capacity += wire_capacity;
                if !retain_wire || *ingress != Some(fragment.interface) {
                    *ingress = None;
                }
            }
        }
        self.ranges.commit_vacant(range);
        self.received += fragment.payload.len();
        self.end = self.end.max(end);
        self.ecn |= 1 << fragment.ecn;
        if !fragment.more {
            self.last = Some(end);
        }
        if self.ecn & 1 != 0 && self.ecn != 1 {
            return Err(InsertError::Discard);
        }
        if self.first.as_ref().is_some_and(|first| {
            self.end + first.header
                > if first.next_field.is_some() {
                    65575
                } else {
                    65535
                }
        }) {
            return Err(InsertError::Oversize);
        }
        if self.charge() > available {
            return Err(InsertError::Discard);
        }
        Ok(())
    }
    fn complete(&self) -> bool {
        self.first.is_some() && self.last == Some(self.received)
    }
    fn finish(self, link: &[u8]) -> Option<(Vec<u8>, Option<Fragments>)> {
        let first = self.first?;
        let length = first.header + self.received;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(link.len() + length).ok()?;
        bytes.extend_from_slice(link);
        let original = match &self.payload {
            Payload::Local(_) => first.quote.as_slice(),
            Payload::Wire { .. } => self.ranges.first_key_value()?.1.wire.as_slice(),
        };
        let next = first
            .next_field
            .map(|field| (field, original[first.header]));
        bytes.extend_from_slice(&original[..first.header]);
        let fragments = match self.payload {
            Payload::Local(payload) => {
                bytes.extend_from_slice(&payload);
                None
            }
            Payload::Wire { capacity, ingress } => {
                for (&start, range) in self.ranges.iter() {
                    let payload_length = usize::from(range.end - start);
                    bytes.extend_from_slice(&range.wire[range.wire.len() - payload_length..]);
                }
                Some(Fragments {
                    ranges: self.ranges,
                    wire_capacity: capacity,
                    ingress,
                })
            }
        };
        packet::normalize(&mut bytes[link.len()..], first.header, next, self.ecn)?;
        Some((bytes, fragments))
    }
}
/// 转发按偏移发送原始分片，检查完整包后把这些分片移交给原转发队列。
pub(super) struct Fragments {
    ranges: FallibleMap<u16, Range>,
    wire_capacity: usize,
    /// 所有被接受的分片共用的合格物理入口。
    pub(super) ingress: Option<u32>,
}
impl Fragments {
    /// 返回偏移最小的待发片的原 IP 字节。
    pub(super) fn first(&self) -> &[u8] {
        &self
            .ranges
            .first_key_value()
            .expect("completed datagram has fragments")
            .1
            .wire
    }
    /// 有后续片时释放已提交的当前片并返回 true，最后一片随整个包释放。
    pub(super) fn advance(&mut self) -> bool {
        if self.ranges.len() == 1 {
            return false;
        }
        let (&key, _) = self
            .ranges
            .first_key_value()
            .expect("completed datagram has fragments");
        let range = self.ranges.remove(&key).expect("first fragment exists");
        self.wire_capacity -= range.wire.capacity();
        true
    }
    /// 返回待发片实际分配的字节和区间节点总大小。
    pub(super) fn charge(&self) -> usize {
        self.wire_capacity + self.ranges.len() * VacantEntry::<u16, Range>::allocation_size()
    }
}
/// 重组完成的唯一完整包；不是设备队列，不产生第二份 packet tap。
pub(super) struct Frame {
    pub(super) bytes: Vec<u8>,
    pub(super) ethernet: bool,
    pub(super) interface: u32,
    /// 只有可能转交目的转换的物理报文才保留原片。
    pub(super) fragments: Option<Fragments>,
}
impl Frame {
    pub(super) fn ip(&self) -> &[u8] {
        &self.bytes[if self.ethernet { 14 } else { 0 }..]
    }
}
/// 可按标准 ICMP 规则响应的原始包引用；所有权随本次处理/超时回收移交。
pub(super) struct Problem {
    pub(super) quote: Vec<u8>,
    pub(super) interface: u32,
    pub(super) reason: (u8, u8, u32),
}
pub(super) enum Outcome {
    Pass,
    Pending,
    Drop,
    Problem(Problem),
    Complete(Frame),
}
/// 转发分片检查结果。收齐后分别移交分类所需的完整报文和待发原片。
pub(super) enum Transit {
    Whole,
    Pending,
    Drop,
    Complete(Vec<u8>, Fragments),
}
pub(super) struct Reassembly {
    packets: FallibleMap<Key, Datagram>,
    // OWNER: 与包表同一事务增删。期限固定，每个在途包恰有一项，无迟到的旧计时记录。
    deadlines: FallibleMap<(Instant, Key), ()>,
    used: usize,
}
impl Reassembly {
    pub(super) fn new() -> Self {
        Self {
            packets: FallibleMap::new(),
            deadlines: FallibleMap::new(),
            used: 0,
        }
    }
    pub(super) fn due(&self, now: Instant) -> bool {
        self.next_deadline().is_some_and(|expires| expires <= now)
    }
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.first_key_value().map(|(&(expires, _), ())| expires)
    }
    /// 仅在真实接收资格内准备分片；无分片立即返回，转发包不进入本机重组。
    pub(super) fn receive(
        &mut self,
        frame: &[u8],
        link: Option<([u8; 6], usize)>,
        interface: u32,
        now: Instant,
        local_translation: [bool; 2],
        admits: impl FnOnce(IpAddr, IpAddr) -> bool,
    ) -> Outcome {
        let ethernet = link.is_some();
        let inspected = packet::inspect(frame, link, interface, false, admits);
        let fragment = match inspected {
            packet::Inspected::Pass => return Outcome::Pass,
            packet::Inspected::Drop => return Outcome::Drop,
            packet::Inspected::Problem(bytes, reason) => return problem(bytes, interface, reason),
            packet::Inspected::Fragment(fragment) => fragment,
        };
        let destination = fragment.key.destination;
        let preserve = local_translation[usize::from(destination.is_ipv6())]
            && link.is_some_and(|(mac, _)| super::device::physical_unicast(frame, mac))
            && !destination.is_multicast()
            && !matches!(destination, IpAddr::V6(address) if address.is_unicast_link_local());
        match self.insert(&fragment, now, (preserve, false)) {
            Ok(None) => Outcome::Pending,
            Ok(Some(datagram)) => datagram
                .finish(if ethernet { &frame[..14] } else { &[] })
                .map_or(Outcome::Drop, |(bytes, fragments)| {
                    Outcome::Complete(Frame {
                        bytes,
                        ethernet,
                        interface,
                        fragments,
                    })
                }),
            Err(InsertError::Oversize) if fragment.key.destination.is_ipv6() => problem(
                fragment.bytes,
                interface,
                (4, 0, (fragment.header + 2) as u32),
            ),
            Err(_) => Outcome::Drop,
        }
    }
    /// 收集已通过转发入口检查的原始分片；畸形或不完整的包不得进入连接跟踪。
    pub(super) fn transit(&mut self, bytes: &[u8], interface: u32, now: Instant) -> Transit {
        let fragment = match packet::inspect(bytes, None, interface, true, |_, _| true) {
            packet::Inspected::Pass => return Transit::Whole,
            packet::Inspected::Drop | packet::Inspected::Problem(..) => return Transit::Drop,
            packet::Inspected::Fragment(fragment) => fragment,
        };
        match self.insert(&fragment, now, (true, true)) {
            Ok(None) => Transit::Pending,
            Ok(Some(datagram)) => match datagram.finish(&[]) {
                Some((bytes, Some(fragments))) => Transit::Complete(bytes, fragments),
                _ => Transit::Drop,
            },
            Err(_) => Transit::Drop,
        }
    }
    fn insert(
        &mut self,
        fragment: &packet::Fragment<'_>,
        now: Instant,
        (retain_wire, transit): (bool, bool),
    ) -> Result<Option<Datagram>, InsertError> {
        let key = fragment.key;
        if !self.packets.contains_key(&key) {
            if self.packets.len() >= DATAGRAMS {
                return Err(InsertError::Discard);
            }
            let packet = Datagram::new(fragment.interface, now, retain_wire);
            if packet.charge() > BYTES - self.used {
                return Err(InsertError::Discard);
            }
            let charge = packet.charge();
            let deadline = FallibleMap::try_prepare((packet.expires, key), ())
                .map_err(|_| InsertError::Discard)?;
            let node = FallibleMap::try_prepare(key, packet).map_err(|_| InsertError::Discard)?;
            self.deadlines.commit_vacant(deadline);
            self.packets.commit_vacant(node);
            self.used += charge;
        }
        let datagram = self
            .packets
            .get_mut(&key)
            .expect("admitted reassembly exists");
        let previous_charge = datagram.charge();
        // RX 与超时消费同一冻结时刻；迟到片不能延长旧包，失败整体退休，不保留半个范围。
        let inserted = if datagram.expires <= now {
            Err(InsertError::Discard)
        } else {
            datagram.insert(
                fragment,
                BYTES - self.used + previous_charge,
                (retain_wire, transit),
            )
        };
        if let Err(error) = inserted {
            self.remove(&key);
            self.used -= previous_charge;
            return Err(error);
        }
        self.used = self.used - previous_charge + datagram.charge();
        if datagram.complete() {
            let datagram = self.remove(&key);
            self.used -= datagram.charge();
            return Ok(Some(datagram));
        }
        Ok(None)
    }
    /// 移交实际包并同时撤下期限。插入失败按调用前计费，完成或超时按当前计费。
    fn remove(&mut self, key: &Key) -> Datagram {
        let packet = self.packets.remove(key).expect("reassembly packet exists");
        assert_eq!(self.deadlines.remove(&(packet.expires, *key)), Some(()));
        packet
    }
    /// 接口退休立即释放旧片；ifindex 不复用，不能把旧状态移交热插入设备。
    pub(super) fn forget(&mut self, interface: u32) {
        self.retain(|key, packet| key.zone != interface && packet.origin != interface);
    }
    /// 停止检查某地址族时丢弃该族未完成的转发包。
    pub(super) fn retain_families(&mut self, ipv4: bool, ipv6: bool) {
        if !ipv4 || !ipv6 {
            self.retain(|key, _| {
                if key.destination.is_ipv4() {
                    ipv4
                } else {
                    ipv6
                }
            });
        }
    }
    fn retain(&mut self, mut keep: impl FnMut(&Key, &Datagram) -> bool) {
        let used = &mut self.used;
        let deadlines = &mut self.deadlines;
        self.packets.retain(|key, packet| {
            let keep = keep(key, packet);
            if !keep {
                assert_eq!(deadlines.remove(&(packet.expires, *key)), Some(()));
                *used -= packet.charge();
            }
            keep
        });
    }
    /// 转发分类器超时只清理缓存，不能以远端收件人的身份报告重组超时。
    pub(super) fn retire_transit(&mut self, now: Instant, work_until: u64) -> bool {
        for _ in 0..RETIRE_BUDGET {
            if self.take_expired(now).is_none() {
                return false;
            }
            if work_until != u64::MAX && crate::time::get_time_us() >= work_until {
                break;
            }
        }
        self.due(now)
    }
    /// 每次移交一个到期包。接口仍启用且已有首片时提供超时引用。
    pub(super) fn retire(
        &mut self,
        now: Instant,
        mut alive: impl FnMut(u32) -> bool,
    ) -> Option<Outcome> {
        let (key, mut packet) = self.take_expired(now)?;
        if alive(packet.origin)
            && let Some(first) = packet.first
        {
            let quote = if matches!(packet.payload, Payload::Wire { .. }) {
                let mut original = packet.ranges.remove(&0).expect("saved first fragment").wire;
                let header = first.header + if first.next_field.is_some() { 8 } else { 0 };
                original.truncate(header.max(1232));
                original
            } else {
                first.quote
            };
            Some(Outcome::Problem(Problem {
                quote,
                interface: packet.origin,
                reason: (if key.destination.is_ipv4() { 11 } else { 3 }, 1, 0),
            }))
        } else {
            Some(Outcome::Drop)
        }
    }
    fn take_expired(&mut self, now: Instant) -> Option<(Key, Datagram)> {
        let (&(expires, key), ()) = self.deadlines.first_key_value()?;
        if expires > now {
            return None;
        }
        let packet = self.remove(&key);
        self.used -= packet.charge();
        Some((key, packet))
    }
}

fn problem(bytes: &[u8], interface: u32, reason: (u8, u8, u32)) -> Outcome {
    let mut quote = Vec::new();
    let bytes = &bytes[..bytes.len().min(1232)];
    if quote.try_reserve_exact(bytes.len()).is_err() {
        return Outcome::Drop;
    }
    quote.extend_from_slice(bytes);
    Outcome::Problem(Problem {
        quote,
        interface,
        reason,
    })
}
