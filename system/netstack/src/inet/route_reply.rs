//! 原codec产生的实际IP replies；所有回复与transport共享同一NET FIB/neighbor dispatch。
use super::NetworkStack;
use alloc::vec::Vec;
use fallible_tree::{FallibleMap, VacantEntry};
use network_protocol::{InetAddress, InetProtocol, SocketError};
use packet_wire::ipv4::Ipv4Packet;
use ::tcp::time::{Duration, Instant};

// 即时协议响应的实际payload/node预算；没有无限延迟队列，不能把64 RX变成无界内存。
const REPLY_BYTES: usize = 256 * 1024;
const REPLY_PACKETS: usize = 64;
// 库neighbor机制拥有ARP准入/静默期；queue这里只调度重试，不另实现neighbor状态机。
const REPLY_RETRY: Duration = Duration::from_millis(1000);
// 旧控制回复不能无限等邻居并占住报文容量和分片编号。
const REPLY_LIFETIME: Duration = Duration::from_millis(5000);
struct Reply {
    packet: Vec<u8>,
    origin: u32,
    retry_at: Instant,
    expires: Instant,
    egress: (Option<u32>, u32),
    // 默认跳数随实际出接口确定。重试可换出口，不能冻结入接口的通告值。
    use_link_hop_limit: bool,
    // OWNER: 当前完整包已提交的 IPv4 载荷，重试只从下一片继续，不另建分片队列。
    offset: usize,
    ipv4_id: Option<super::identification::Lease>,
}
/// 实际接口的唯一主机 ICMP 错误桶；IPv4/IPv6 重组与 IPv6 端口错误共享原有限速。
pub(in crate::inet) struct ErrorBudget {
    tokens: u8,
    refill: u64,
}
impl ErrorBudget {
    pub(in crate::inet) fn new() -> Self {
        Self {
            tokens: 10,
            refill: 0,
        }
    }
    /// 同一接口 IP 错误桶；重组错误与端口不可达共用，不把可伪造请求放大为无界回复。
    pub(in crate::inet) fn spend(&mut self, now: u64) -> bool {
        let periods = now.saturating_sub(self.refill) / 100;
        if periods != 0 {
            self.tokens = (u64::from(self.tokens) + periods).min(10) as u8;
            // 保留不足一个 token 的余数，不能在持续流量下反复丢弃积累时间。
            self.refill = now - now.saturating_sub(self.refill) % 100;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
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
    /// 在唯一回复队列的预算内编码一份原生 IPv4 回复。
    pub(super) fn enqueue_ipv4(
        &mut self,
        header: packet_wire::ipv4::Ipv4Header<'_>,
        payload_length: usize,
        origin: u32,
        now: Instant,
        emit: impl FnOnce(&mut [u8]) -> bool,
    ) -> bool {
        let offset = 20 + header.options.len();
        let length = offset + payload_length;
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
        if !emit(&mut packet[offset..]) || header.emit(&mut packet).is_err() {
            return false;
        }
        self.enqueue_packet(packet, origin, (None, 0), now, false)
    }
    /// 为 native TCP 的立即控制回复准备独占完整 IP 字节；不保留连接或载荷引用。
    pub(super) fn enqueue_tcp(
        &mut self,
        ip: ::tcp::IpPacket,
        segment: ::tcp::Segment<'_>,
        egress: (Option<u32>, u32),
        origin: u32,
        now: Instant,
        key: ::tcp::SigningKey<'_>,
    ) -> bool {
        let header = if ip.src_addr().is_ipv4() { 20 } else { 40 };
        let length = header + segment.buffer_len();
        let overhead = VacantEntry::<u64, Reply>::allocation_size();
        if self.packets.len() >= REPLY_PACKETS
            || self
                .used
                .checked_add(length)
                .and_then(|size| size.checked_add(overhead))
                .is_none_or(|size| size > REPLY_BYTES)
        {
            return false;
        }
        let mut packet = Vec::new();
        if packet.try_reserve_exact(length).is_err() {
            return false;
        }
        let Some(charge) = packet.capacity().checked_add(overhead) else {
            return false;
        };
        if self
            .used
            .checked_add(charge)
            .is_none_or(|size| size > REPLY_BYTES)
        {
            return false;
        }
        packet.resize(length, 0);
        let encoded = segment.emit_with_authentication(
            &mut packet[header..],
            (ip.src_addr(), ip.dst_addr()),
            key,
            false,
        );
        if encoded.is_err() {
            return false;
        }
        let encoded = match (ip.src_addr(), ip.dst_addr()) {
            (core::net::IpAddr::V4(source), core::net::IpAddr::V4(destination)) => {
                packet_wire::ipv4::Ipv4Header {
                    source,
                    destination,
                    protocol: 6,
                    hop_limit: ip.hop_limit().unwrap_or(super::DEFAULT_IPV4_HOP_LIMIT),
                    dscp_ecn: 0,
                    identification: 0,
                    fragment: 0x4000,
                    options: &[],
                }
                .emit(&mut packet)
            }
            (core::net::IpAddr::V6(source), core::net::IpAddr::V6(destination)) => {
                packet_wire::ipv6::Ipv6Header {
                    source,
                    destination,
                    next_header: 6,
                    hop_limit: ip.hop_limit().unwrap_or(super::ipv6::DEFAULT_HOP_LIMIT),
                    traffic_class: 0,
                    flow_label: 0,
                }
                .emit(&mut packet)
            }
            _ => return false,
        };
        if encoded.is_err() {
            return false;
        }
        self.enqueue_packet(
            packet,
            origin,
            egress,
            now,
            ip.src_addr().is_ipv6() && ip.hop_limit().is_none(),
        )
    }
    /// 转发产生的本机错误沿现有回复队列发送；冷路径只复制一次，无第二份 IP 状态。
    pub(super) fn enqueue_copy(
        &mut self,
        bytes: &[u8],
        origin: u32,
        bound: Option<u32>,
        now: Instant,
    ) -> bool {
        let overhead = VacantEntry::<u64, Reply>::allocation_size();
        if self.packets.len() >= REPLY_PACKETS
            || self
                .used
                .checked_add(bytes.len())
                .and_then(|size| size.checked_add(overhead))
                .is_none_or(|size| size > REPLY_BYTES)
        {
            return false;
        }
        let mut packet = Vec::new();
        if packet.try_reserve_exact(bytes.len()).is_err() {
            return false;
        }
        packet.extend_from_slice(bytes);
        self.enqueue_packet(
            packet,
            origin,
            (bound, 0),
            now,
            bytes.first().is_some_and(|byte| byte >> 4 == 6),
        )
    }
    /// 消费原生 IP 错误/回复的独占完整字节；失败立即释放，不复制到第二个等待队列。
    pub(super) fn enqueue_packet(
        &mut self,
        packet: Vec<u8>,
        origin: u32,
        egress: (Option<u32>, u32),
        now: Instant,
        use_link_hop_limit: bool,
    ) -> bool {
        let Some(charge) = packet
            .capacity()
            .checked_add(VacantEntry::<u64, Reply>::allocation_size())
        else {
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
        let Ok(node) = FallibleMap::try_prepare(
            crate::identity::next(),
            Reply {
                packet,
                origin,
                retry_at: now,
                expires: now + REPLY_LIFETIME,
                egress,
                use_link_hop_limit,
                offset: 0,
                ipv4_id: None,
            },
        ) else {
            return false;
        };
        self.packets.commit_vacant(node);
        self.used += charge;
        true
    }
    fn remove(&mut self, key: u64, ids: &mut super::identification::Identification, now: Instant) {
        let reply = self
            .packets
            .remove(&key)
            .expect("routed reply retired twice");
        if let Some(lease) = reply.ipv4_id {
            ids.release(lease, now);
        }
        self.used = self
            .used
            .checked_sub(reply.packet.capacity() + VacantEntry::<u64, Reply>::allocation_size())
            .expect("routed reply charge double release");
    }
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.packets.values().map(|reply| reply.retry_at).min()
    }
}
impl NetworkStack {
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
        let mut backlog = false;
        while let Some((&key, _)) = match cursor {
            Some(key) => self.route_replies.packets.successor(&key),
            None => self.route_replies.packets.first_key_value(),
        } {
            cursor = Some(key);
            let reply = self
                .route_replies
                .packets
                .get_mut(&key)
                .expect("selected reply");
            if reply.retry_at > now {
                continue;
            }
            let origin = reply.origin;
            if reply.expires <= now {
                if let Ok(interface) = self.interface(origin) {
                    interface.device.record_transmit_drop();
                }
                self.route_replies
                    .remove(key, &mut self.identification, now);
                continue;
            }
            let packet = &mut reply.packet;
            if packet[0] >> 4 == 4 && packet[6] & 0x40 == 0 && reply.ipv4_id.is_none() {
                match self.identification.reserve_header(packet, now) {
                    Ok(lease) => reply.ipv4_id = Some(lease),
                    Err(_) => {
                        if let Ok(interface) = self.interface(origin) {
                            interface.device.record_transmit_drop();
                        }
                        self.route_replies
                            .remove(key, &mut self.identification, now);
                        continue;
                    }
                }
            }
            let reply = &self.route_replies.packets[&key];
            if reply.retry_at > now {
                continue;
            }
            let (source, destination, tcp) = if reply.packet[0] >> 4 == 4 {
                let packet =
                    Ipv4Packet::parse_header(&reply.packet).expect("owned IPv4 reply");
                (
                    core::net::IpAddr::V4(packet.source()),
                    core::net::IpAddr::V4(packet.destination()),
                    (packet.protocol() == 6).then(|| packet.payload()),
                )
            } else {
                let packet =
                    packet_wire::ipv6::Ipv6Packet::parse(&reply.packet).expect("owned IPv6 reply");
                (
                    core::net::IpAddr::V6(packet.source()),
                    core::net::IpAddr::V6(packet.destination()),
                    (packet.upper_protocol() == 6)
                        .then(|| packet.upper_layer())
                        .flatten(),
                )
            };
            // 已关闭的本机连接不保留端口。延迟的 RST 不能终止后来占用同一元组的转发连接。
            let conflicts = tcp
                .and_then(|bytes| bytes.first_chunk::<4>())
                .is_some_and(|ports| {
                    self.forwarding.local_tuple_taken(
                        InetProtocol::Tcp,
                        (
                            InetAddress::new(source, u16::from_be_bytes([ports[0], ports[1]])),
                            InetAddress::new(destination, u16::from_be_bytes([ports[2], ports[3]])),
                        ),
                        now,
                    )
                });
            let route = if conflicts {
                Err(SocketError::AddressInUse)
            } else {
                self.ip_output(source, InetAddress::new(destination, 0), reply.egress, now)
            };
            let route = match route {
                Ok(route) => route,
                Err(_) => {
                    if let Ok(interface) = self.interface(reply.origin) {
                        interface.device.record_transmit_drop();
                    }
                    self.route_replies
                        .remove(key, &mut self.identification, now);
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
            let reply = self
                .route_replies
                .packets
                .get_mut(&key)
                .expect("live reply");
            if reply.use_link_hop_limit {
                reply.packet[7] = route.hop_limit;
            }
            let ipv4 = reply.packet[0] >> 4 == 4;
            let (sent, progress) = if ipv4 && (reply.packet.len() > route.mtu || reply.offset != 0)
            {
                let header_length = usize::from(reply.packet[0] & 15) * 4;
                let mut header = [0; 60];
                match packet_wire::ipv4::fragment::header(
                    &reply.packet[..header_length],
                    reply.packet.len() - header_length,
                    reply.offset,
                    route.mtu,
                    &mut header,
                ) {
                    Ok(range) => {
                        let sent = interface.device.transmit_ipv4(
                            (&mut interface.arp, interface.state, &mut interface.ipv6),
                            &header[..header_length],
                            (
                                range.len(),
                                |body: &mut [u8]| {
                                    body.copy_from_slice(
                                        &reply.packet
                                            [header_length + range.start..header_length + range.end],
                                    )
                                },
                                None,
                            ),
                            (route.neighbor, route.logical_interface_index),
                            now,
                            (false, |_| 0),
                        );
                        (sent, Some((range.end, reply.packet.len() - header_length)))
                    }
                    Err(_) => (
                        Err(device_model::network::NetworkError::FrameTooLarge),
                        None,
                    ),
                }
            } else {
                (interface.transmit_packet(&reply.packet, &route, now), None)
            };
            match sent {
                Err(device_model::network::NetworkError::WouldBlock) => {
                    let reply = self
                        .route_replies
                        .packets
                        .get_mut(&key)
                        .expect("live reply");
                    reply.retry_at = (now + REPLY_RETRY).min(reply.expires);
                }
                Err(_) => {
                    interface.device.record_transmit_drop();
                    self.route_replies
                        .remove(key, &mut self.identification, now);
                }
                Ok(()) => {
                    super::route::probe_gateway(&mut self.interfaces, route.recovery, now);
                    if let Some((offset, total)) = progress
                        && offset < total
                    {
                        self.route_replies
                            .packets
                            .get_mut(&key)
                            .expect("live reply")
                            .offset = offset;
                        backlog = true;
                    } else {
                        self.route_replies
                            .remove(key, &mut self.identification, now);
                    }
                }
            }
        }
        backlog
    }
}
