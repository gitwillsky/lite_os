//! 原codec产生的实际IP replies；所有回复与transport共享同一NET FIB/neighbor dispatch。
use super::NetworkStack;
use crate::fallible_tree::{FallibleMap, VacantEntry};
use alloc::vec::Vec;
use smoltcp::{
    iface::Ipv4Reply,
    phy::Device,
    time::{Duration, Instant},
    wire::Ipv4Packet,
};

// 即时协议响应的实际payload/node预算；没有无限延迟队列，不能把64 RX变成无界内存。
const REPLY_BYTES: usize = 256 * 1024;
const REPLY_PACKETS: usize = 64;
// 库neighbor机制拥有ARP准入/静默期；queue这里只调度重试，不另实现neighbor状态机。
const REPLY_RETRY: Duration = Duration::from_millis(1000);
struct Reply {
    packet: Vec<u8>,
    origin: u32,
    retry_at: Instant,
    bound_interface: Option<u32>,
}
pub(super) struct RouteReplies {
    packets: FallibleMap<u64, Reply>,
    used: usize,
}
impl RouteReplies {
    pub(super) fn new() -> Self {
        Self {
            packets: FallibleMap::new(),
            used: 0,
        }
    }
    pub(super) fn enqueue(&mut self, reply: &Ipv4Reply<'_>, origin: u32, now: Instant) -> bool {
        let length = reply.buffer_len();
        let Some(charge) = length.checked_add(VacantEntry::<u64, Reply>::allocation_size()) else {
            return false;
        };
        if self.packets.len() >= REPLY_PACKETS
            || self
                .used
                .checked_add(charge)
                .is_none_or(|used| used > REPLY_BYTES)
        {
            return false;
        }
        let mut packet = Vec::new();
        if packet.try_reserve_exact(length).is_err() {
            return false;
        }
        let Some(charge) = packet
            .capacity()
            .checked_add(VacantEntry::<u64, Reply>::allocation_size())
        else {
            return false;
        };
        if self
            .used
            .checked_add(charge)
            .is_none_or(|used| used > REPLY_BYTES)
        {
            return false;
        }
        packet.resize(length, 0);
        reply.emit(&mut packet);
        let Ok(prepared) = FallibleMap::try_prepare(
            crate::id::next_runtime_object_id(),
            Reply {
                packet,
                origin,
                retry_at: now,
                bound_interface: reply.bound_interface(),
            },
        ) else {
            return false;
        };
        self.packets.commit_vacant(prepared);
        self.used += charge;
        true
    }
    fn remove(&mut self, key: u64) {
        let reply = self
            .packets
            .remove(&key)
            .expect("routed reply retired twice");
        self.used = self
            .used
            .checked_sub(reply.packet.capacity() + VacantEntry::<u64, Reply>::allocation_size())
            .expect("routed reply charge double release");
    }
    pub(super) fn due(&self, now: Instant) -> bool {
        self.packets.iter().any(|(_, reply)| reply.retry_at <= now)
    }
}
impl NetworkStack {
    /// 关闭的transport只发布一次原codec reset；报文预算/OOM/无route均不保留TCP身份。
    /// 普通FIN/TIME_WAIT仍由实际socket时钟拥有，不进入此报文队列。
    pub(super) fn queue_tcp_resets(&mut self, now: Instant) {
        for (_, socket) in self.sockets.iter_mut() {
            let smoltcp::socket::Socket::Tcp(socket) = socket else {
                continue;
            };
            let Some(packet) = socket.take_reset() else {
                continue;
            };
            let Ok(route) = self.routing.select(
                super::from_ip(packet.0.dst_addr()),
                socket.bound_interface(),
                &self.interfaces,
            ) else {
                // native tcp_send_active_reset是best-effort单次发布，不能因无route持端口。
                continue;
            };
            let origin = route.interface_index;
            let reply = Ipv4Reply::tcp_reset(packet, socket.bound_interface());
            if !self.route_replies.enqueue(&reply, origin, now) {
                self.interfaces
                    .iter()
                    .find(|interface| interface.index == origin)
                    .expect("FIB returned absent device under NET guard")
                    .device
                    .record_transmit_drop();
            }
        }
    }
    /// 每批真实设备消费同一RIB reply计划；最多64个实际records，发送最多8帧。
    pub(super) fn poll_route_replies(
        &mut self,
        now: Instant,
        first: usize,
        end: usize,
        rotation: usize,
    ) -> bool {
        let mut cursor = None;
        let mut remaining = 8usize;
        while let Some((&key, _)) = match cursor {
            Some(key) => self.route_replies.packets.successor(&key),
            None => self.route_replies.packets.first_key_value(),
        } {
            cursor = Some(key);
            let reply = &self.route_replies.packets[&key];
            if reply.retry_at > now {
                continue;
            }
            let packet = Ipv4Packet::new_checked(reply.packet.as_slice())
                .expect("reply codec emitted malformed IP");
            let route = match self.routing.select(
                packet.dst_addr(),
                reply.bound_interface,
                &self.interfaces,
            ) {
                Ok(route) => route,
                Err(_) => {
                    if let Ok(interface) = self.interface(reply.origin) {
                        interface.device.record_transmit_drop();
                    }
                    self.route_replies.remove(key);
                    continue;
                }
            };
            let count = self.interfaces.len();
            let Some(position) = (first..end)
                .map(|cursor| (rotation + cursor) % count)
                .find(|position| self.interfaces[*position].index == route.interface_index)
            else {
                continue;
            };
            if remaining == 0 {
                return true;
            }
            remaining -= 1;
            let interface = &mut self.interfaces[position];
            if reply.packet.len() > interface.device.ip_mtu() {
                interface.device.record_transmit_drop();
                self.route_replies.remove(key);
                continue;
            }
            let sent = if let Some(token) = interface.device.transmit(now) {
                interface.interface.dispatch_ipv4_packet(
                    token,
                    &reply.packet,
                    route.next_hop,
                    route.logical_interface_index,
                )
            } else {
                false
            };
            if sent {
                self.route_replies.remove(key);
            } else {
                self.route_replies
                    .packets
                    .get_mut(&key)
                    .expect("reply changed under NET guard")
                    .retry_at = now + REPLY_RETRY;
            }
        }
        false
    }
}
