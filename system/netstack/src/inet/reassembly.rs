//! 同一 NET 的双栈本机重组；分片区间与载荷只有此 owner，完整包移交原接收路径。
mod packet;
pub(super) mod reply;
use alloc::vec::Vec;
use core::net::IpAddr;
use fallible_tree::{FallibleMap, VacantEntry};
use smoltcp::time::Instant;

// OWNER: 仅实际分片才占容量；限制敌对乱序流量的堆占用和每轮回收工作，不预分配大缓冲池。
const DATAGRAMS: usize = 128;
const BYTES: usize = 4 * 1024 * 1024;
const LIFETIME_MS: i64 = 60_000;
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
struct Datagram {
    ranges: FallibleMap<u16, Range>,
    payload: Vec<u8>,
    first: Option<First>,
    last: Option<usize>,
    received: usize,
    ecn: u8,
    origin: u32,
    expires: Instant,
}
impl Datagram {
    fn charge(&self) -> usize {
        VacantEntry::<Key, Self>::allocation_size()
            + self.ranges.len() * VacantEntry::<u16, Range>::allocation_size()
            + self.payload.capacity()
            + self
                .first
                .as_ref()
                .map_or(0, |first| first.quote.capacity())
    }
    fn new(origin: u32, now: Instant) -> Self {
        Self {
            ranges: FallibleMap::new(),
            payload: Vec::new(),
            first: None,
            last: None,
            received: 0,
            ecn: 0,
            origin,
            expires: now + smoltcp::time::Duration::from_millis(LIFETIME_MS as u64),
        }
    }
    /// 区间 AVL 只检查相邻范围；乱序/重叠不触发线性移动或重复写载荷。
    fn insert(
        &mut self,
        fragment: &packet::Fragment<'_>,
        available: usize,
    ) -> Result<(), InsertError> {
        let start = fragment.offset;
        let end = start + fragment.payload.len();
        if fragment.payload.is_empty()
            || self
                .last
                .is_some_and(|last| end > last || fragment.more && end == last)
            || !fragment.more
                && (self.last.is_some_and(|last| end != last) || self.payload.len() > end)
        {
            return Err(InsertError::Discard);
        }
        if let Some(previous) = self.ranges.get(&(start as u16)) {
            return if usize::from(previous.end) == end
                && previous.more == fragment.more
                && self.payload[start..end] == *fragment.payload
            {
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
            fragment.bytes.len().min(fragment.data.max(1232))
        } else {
            0
        };
        // 倍增而非逐片精确扩容；否则八字节小片会造成二次方的载荷搬移。
        let capacity = if end > self.payload.capacity() {
            end.max(self.payload.capacity().saturating_mul(2))
                .min(65535)
        } else {
            self.payload.capacity()
        };
        let required = capacity
            + first_length.max(self.first.as_ref().map_or(0, |f| f.quote.capacity()))
            + VacantEntry::<Key, Self>::allocation_size()
            + (self.ranges.len() + 1) * VacantEntry::<u16, Range>::allocation_size();
        if required > available {
            return Err(InsertError::Discard);
        }
        let range = FallibleMap::try_prepare(
            start as u16,
            Range {
                end: end as u16,
                more: fragment.more,
            },
        )
        .map_err(|_| InsertError::Discard)?;
        if end > self.payload.len() {
            self.payload
                .try_reserve_exact(capacity - self.payload.len())
                .map_err(|_| InsertError::Discard)?;
            self.payload.resize(end, 0);
        }
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
        self.payload[start..end].copy_from_slice(fragment.payload);
        self.ranges.commit_vacant(range);
        self.received += fragment.payload.len();
        self.ecn |= 1 << fragment.ecn;
        if !fragment.more {
            self.last = Some(end);
        }
        if self.ecn & 1 != 0 && self.ecn != 1 {
            return Err(InsertError::Discard);
        }
        if self.first.as_ref().is_some_and(|first| {
            self.payload.len() + first.header
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
    fn finish(self, link: &[u8], interface: u32) -> Option<Frame> {
        let first = self.first?;
        let length = first.header + self.received;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(link.len() + length).ok()?;
        bytes.extend_from_slice(link);
        bytes.extend_from_slice(&first.quote[..first.header]);
        bytes.extend_from_slice(&self.payload);
        packet::normalize(&mut bytes[link.len()..], &first, self.ecn)?;
        Some(Frame {
            bytes,
            ethernet: !link.is_empty(),
            interface,
        })
    }
}
/// 重组完成的唯一完整包；不是设备队列，不产生第二份 packet tap。
pub(super) struct Frame {
    pub(super) bytes: Vec<u8>,
    pub(super) ethernet: bool,
    pub(super) interface: u32,
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
pub(super) struct Reassembly {
    packets: FallibleMap<Key, Datagram>,
    used: usize,
}
impl Reassembly {
    pub(super) fn new() -> Self {
        Self {
            packets: FallibleMap::new(),
            used: 0,
        }
    }
    pub(super) fn due(&self, now: Instant) -> bool {
        self.packets.values().any(|packet| packet.expires <= now)
    }
    /// 仅在真实接收资格内准备分片；无分片立即返回，转发包不进入本机重组。
    pub(super) fn receive(
        &mut self,
        frame: &[u8],
        link: Option<([u8; 6], usize)>,
        interface: u32,
        now: Instant,
        admits: impl FnOnce(IpAddr, IpAddr) -> bool,
    ) -> Outcome {
        let ethernet = link.is_some();
        let inspected = packet::inspect(frame, link, interface, admits);
        let fragment = match inspected {
            packet::Inspected::Pass => return Outcome::Pass,
            packet::Inspected::Drop => return Outcome::Drop,
            packet::Inspected::Problem(bytes, reason) => return problem(bytes, interface, reason),
            packet::Inspected::Fragment(fragment) => fragment,
        };
        let key = fragment.key;
        if !self.packets.contains_key(&key) {
            if self.packets.len() >= DATAGRAMS {
                return Outcome::Drop;
            }
            let packet = Datagram::new(interface, now);
            if packet.charge() > BYTES - self.used {
                return Outcome::Drop;
            }
            let charge = packet.charge();
            let Ok(node) = FallibleMap::try_prepare(key, packet) else {
                return Outcome::Drop;
            };
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
            datagram.insert(&fragment, BYTES - self.used + previous_charge)
        };
        if let Err(error) = inserted {
            self.packets.remove(&key);
            self.used -= previous_charge;
            return if matches!(error, InsertError::Oversize) && key.destination.is_ipv6() {
                problem(
                    fragment.bytes,
                    interface,
                    (4, 0, (fragment.header + 2) as u32),
                )
            } else {
                Outcome::Drop
            };
        }
        self.used = self.used - previous_charge + datagram.charge();
        if datagram.complete() {
            let datagram = self
                .packets
                .remove(&key)
                .expect("complete reassembly exists");
            self.used -= datagram.charge();
            return datagram
                .finish(if ethernet { &frame[..14] } else { &[] }, interface)
                .map_or(Outcome::Drop, Outcome::Complete);
        }
        Outcome::Pending
    }
    /// 接口退休立即释放旧片；ifindex 不复用，不能把旧状态移交热插入设备。
    pub(super) fn forget(&mut self, interface: u32) {
        loop {
            let key = self.packets.iter().find_map(|(key, packet)| {
                (key.zone == interface || packet.origin == interface).then_some(*key)
            });
            let Some(key) = key else {
                return;
            };
            let packet = self
                .packets
                .remove(&key)
                .expect("selected reassembly exists");
            self.used -= packet.charge();
        }
    }
    /// 每次移交一个到期/失效包；外层共享轮次时刻与预算，未到期的旧片不刷新期限。
    pub(super) fn retire(
        &mut self,
        now: Instant,
        mut alive: impl FnMut(u32) -> bool,
    ) -> Option<Outcome> {
        let key = self.packets.iter().find_map(|(key, packet)| {
            (packet.expires <= now || !alive(packet.origin)).then_some(*key)
        })?;
        let packet = self
            .packets
            .remove(&key)
            .expect("selected reassembly exists");
        self.used -= packet.charge();
        if packet.expires <= now
            && alive(packet.origin)
            && let Some(first) = packet.first
        {
            Some(Outcome::Problem(Problem {
                quote: first.quote,
                interface: packet.origin,
                reason: (if key.destination.is_ipv4() { 11 } else { 3 }, 1, 0),
            }))
        } else {
            Some(Outcome::Drop)
        }
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
