//! IPv6 邻居报文、链路计时参数和本机回复队列。可达性与时钟由双栈邻居表维护。
use super::{Packet, solicited};
use crate::inet::neighbor::{NeighborCache, Progress};
use alloc::vec::Vec;
use core::net::Ipv6Addr;
use fallible_tree::{FallibleMap, VacantEntry};

const PACKETS: usize = 64;
const BYTES: usize = 256 * 1024;
const WORK: usize = 8;
const PACKET_LIFETIME: u64 = 5000;

struct Pending {
    // 同一报文准入时选中的邻居，不是 IPv6 远端；载荷/出口冻结至发送或五秒退休，不是每流路由缓存。
    destination: Ipv6Addr,
    frame: Vec<u8>,
    expires: u64,
    // 发送容量不足后只等实际活动；缺失时已解析的队列会让 timer 持续空转。
    writable: bool,
}

pub(super) struct Neighbors {
    cache: NeighborCache<Ipv6Addr>,
    packets: FallibleMap<u64, Pending>,
    used: usize,
}
impl Neighbors {
    pub(super) fn new() -> Self {
        Self {
            cache: NeighborCache::new(),
            packets: FallibleMap::new(),
            used: 0,
        }
    }
    pub(super) fn apply_timers(&mut self, reachable_ms: u32, retransmit_ms: u32) {
        self.cache.apply_timers(reachable_ms, retransmit_ms);
    }
    pub(super) fn retransmit_ms(&self) -> u64 {
        self.cache.retransmit_ms()
    }
    pub(super) fn rows(
        &self,
        interface_index: u32,
    ) -> impl Iterator<Item = network_protocol::Neighbor> + Clone {
        self.cache.rows(interface_index)
    }
    pub(super) fn clear(&mut self) -> usize {
        let count = self.packets.len();
        *self = Self::new();
        count
    }
    pub(super) fn take_reachability_change(&mut self) -> bool {
        self.cache.take_reachability_change()
    }
    pub(super) fn learn(&mut self, address: Ipv6Addr, mac: [u8; 6], now: u64) {
        if !address.is_unspecified() && !address.is_multicast() {
            let packets = &self.packets;
            self.cache.learn(address, mac, now, |victim| {
                packets
                    .iter()
                    .any(|(_, packet)| packet.destination == victim)
            });
        }
    }
    pub(super) fn learn_router(&mut self, address: Ipv6Addr, mac: Option<[u8; 6]>, now: u64) {
        if let Some(mac) = mac {
            self.learn(address, mac, now);
        }
        self.cache.mark_router(address);
    }
    pub(super) fn router_rank(&self, address: Ipv6Addr) -> (bool, bool) {
        self.cache.router_rank(address)
    }
    pub(super) fn router_probe_due(&self, address: Ipv6Addr, now: u64) -> bool {
        self.cache.router_probe_due(address, now)
    }
    pub(super) fn request_router_probe(&mut self, address: Ipv6Addr, now: u64) {
        self.cache.request_router_probe(address, now);
    }
    pub(super) fn monitor_failed_gateway(&mut self, address: Ipv6Addr, enable: bool) {
        self.cache.monitor_failed_gateway(address, enable);
    }
    /// 按 RFC 4861 §7.2.5 处理已验收 NA，报告被接受的 R=0。
    pub(super) fn advertise(
        &mut self,
        address: Ipv6Addr,
        mac: Option<[u8; 6]>,
        flags: u32,
        now: u64,
    ) -> bool {
        let router = flags & 0x8000_0000 != 0;
        self.cache.confirm(
            address,
            mac,
            flags & 0x4000_0000 != 0,
            flags & 0x2000_0000 != 0,
            router,
            now,
        ) && !router
    }
    pub(super) fn resolve(
        &mut self,
        address: Ipv6Addr,
        now: u64,
    ) -> Result<Option<[u8; 6]>, network_protocol::SocketError> {
        let packets = &self.packets;
        self.cache.resolve(address, now, |victim| {
            packets
                .iter()
                .any(|(_, packet)| packet.destination == victim)
        })
    }
    fn charge(frame: &Vec<u8>) -> usize {
        frame.capacity() + VacantEntry::<u64, Pending>::allocation_size()
    }
    fn take_packet(&mut self, id: u64) -> Pending {
        let packet = self
            .packets
            .remove(&id)
            .expect("queued packet retired once");
        self.used = self
            .used
            .checked_sub(Self::charge(&packet.frame))
            .expect("queued IPv6 byte charge refunded once");
        packet
    }
    /// 已解析快路径直接编码 DMA；仅邻居未知才捕获载荷，所有分配成功后共同发布解析和等待义务。
    pub(super) fn send(
        &mut self,
        packet: Packet<'_>,
        neighbor: Option<Ipv6Addr>,
        mac: [u8; 6],
        now: u64,
        send: &mut impl FnMut(Packet<'_>) -> bool,
        dropped: &mut impl FnMut(),
    ) {
        let Some(destination) = neighbor else {
            if !send(packet) {
                dropped();
            }
            return;
        };
        if let Some(mac) = self.cache.mac(destination, now) {
            if !send(packet.resolved(mac)) {
                dropped();
            }
            return;
        }
        let length = packet.length();
        if self.packets.len() >= PACKETS
            || length + VacantEntry::<u64, Pending>::allocation_size()
                > BYTES.saturating_sub(self.used)
        {
            dropped();
            return;
        }
        let mut frame = Vec::new();
        if frame.try_reserve_exact(length).is_err()
            || Self::charge(&frame) > BYTES.saturating_sub(self.used)
        {
            dropped();
            return;
        }
        frame.resize(length, 0);
        if packet.emit(&mut frame, mac).is_err() {
            dropped();
            return;
        }
        let charge = Self::charge(&frame);
        let Ok(entry) = FallibleMap::try_prepare(
            crate::identity::next(),
            Pending {
                destination,
                frame,
                expires: now.saturating_add(PACKET_LIFETIME),
                writable: false,
            },
        ) else {
            dropped();
            return;
        };
        let packets = &self.packets;
        if !self.cache.ensure(destination, now, |victim| {
            packets
                .iter()
                .any(|(_, packet)| packet.destination == victim)
        }) {
            dropped();
            return;
        }
        self.packets.commit_vacant(entry);
        self.used += charge;
        self.request_router_probe(destination, now);
    }

    pub(super) fn due(&self, now: u64) -> bool {
        self.cache.due(now)
            || self.packets.iter().any(|(_, packet)| {
                packet.expires <= now
                    || !packet.writable && self.cache.router_rank(packet.destination).1
            })
    }
    pub(super) fn poll(
        &mut self,
        local: Ipv6Addr,
        now: u64,
        send: &mut impl FnMut(Packet<'_>) -> bool,
        dropped: &mut impl FnMut(),
        is_gateway: impl Fn(Ipv6Addr) -> bool,
        monitored_gateway: impl Fn(Ipv6Addr) -> bool,
    ) -> Progress<Ipv6Addr> {
        let mut progress = self.cache.poll(
            Ipv6Addr::UNSPECIFIED,
            now,
            |target, mac| {
                let destination = if mac.is_some() {
                    target
                } else {
                    solicited(target)
                };
                send(Packet::Solicitation {
                    source: local,
                    target,
                    destination,
                    destination_mac: mac
                        .unwrap_or_else(|| super::packet::multicast_mac(destination)),
                })
            },
            is_gateway,
            monitored_gateway,
        );
        for destination in progress.failed.iter().flatten() {
            while let Some(id) = self
                .packets
                .iter()
                .find(|(_, packet)| &packet.destination == destination)
                .map(|(&id, _)| id)
            {
                self.take_packet(id);
                dropped();
            }
        }
        let mut cursor = None;
        let mut work = 0;
        while work < WORK {
            let next = match cursor {
                Some(id) => self.packets.successor(&id),
                None => self.packets.first_key_value(),
            }
            .map(|(&id, _)| id);
            let Some(id) = next else {
                break;
            };
            cursor = Some(id);
            let packet = &self.packets[&id];
            if packet.expires <= now {
                self.take_packet(id);
                dropped();
                work += 1;
                continue;
            }
            let address = packet.destination;
            let Some(mac) = self.cache.mac(address, now) else {
                continue;
            };
            let packet = &self.packets[&id];
            if send(Packet::Buffered {
                frame: &packet.frame,
                destination_mac: mac,
            }) {
                self.take_packet(id);
            } else {
                self.packets.get_mut(&id).unwrap().writable = true;
            }
            work += 1;
        }
        progress.backlog |= self.due(now);
        progress
    }
}
