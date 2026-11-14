//! 本机 IPv4 准入及 ICMP 回复。报文字节借用原 RX，回复由 NET 的有界队列拥有。

use crate::inet::{
    DEFAULT_IPV4_HOP_LIMIT,
    forward::ReceiveHeader,
    route_reply::{ErrorBudget, RouteReplies},
};
use core::net::Ipv4Addr;
use device_model::network::NetworkReceiveFrame;
use network_protocol::RouteMetrics;
use packet_wire::{
    batch::NetworkPacket,
    ethernet::EthernetFrame,
    icmpv4,
    ipv4::{Ipv4Header, Ipv4Packet},
};
use ::tcp::time::Instant;

/// 同次 FIB 查找提供的接收地址和反向路径。
pub(in crate::inet) struct Ipv4IngressFacts {
    pub(in crate::inet) specific_destination: Option<Ipv4Addr>,
    pub(in crate::inet) reverse_route: Option<(usize, RouteMetrics)>,
    pub(in crate::inet) bound_reverse_route: Option<(usize, RouteMetrics)>,
}

/// 已完成本机准入的完整原生 IPv4 数据报。传输协议独立校验自己的首部与校验和。
#[derive(Clone, Copy)]
pub(in crate::inet) struct Ipv4Ingress<'a> {
    pub(in crate::inet) ip: Ipv4Packet<'a>,
    pub(in crate::inet) facts: &'a Ipv4IngressFacts,
    pub(in crate::inet) interface_index: u32,
    pub(in crate::inet) multicast: bool,
    pub(in crate::inet) checksum_valid: bool,
    pub(in crate::inet) handled_by_raw: bool,
}

/// 返回同帧的原生 IP 视图和链路组地址事实。IP-medium 输入已由设备筛选链路协议。
pub(super) fn decode<'a>(
    frame: &'a [u8],
    prepared: Option<NetworkPacket<'a>>,
    original: Option<&NetworkReceiveFrame<'_>>,
    mac: Option<[u8; 6]>,
) -> Option<(Ipv4Packet<'a>, bool)> {
    let (payload, group) = if let Some(mac) = mac {
        let link = EthernetFrame::parse(frame).ok()?;
        let group = link.destination()[0] & 1 != 0;
        if link.type_or_length() != 0x0800 || !group && link.destination() != mac {
            return None;
        }
        (link.payload(), group)
    } else {
        (frame, false)
    };
    let ip = match prepared {
        Some(NetworkPacket::Ipv4(ip)) => ip,
        Some(_) => return None,
        None => Ipv4Packet::parse_header(payload).ok()?,
    };
    if ip.fragmented() || !ReceiveHeader::from_frame(original).ipv4_valid(ip) {
        return None;
    }
    Some((ip, group))
}

pub(super) fn echo_reply(packet: Ipv4Ingress<'_>, replies: &mut RouteReplies, now: Instant) -> bool {
    let ip = packet.ip;
    if packet.multicast || ip.source().is_unspecified() || !ip.payload().starts_with(&[8, 0]) {
        return true;
    }
    if icmpv4::Icmpv4Packet::parse(ip.payload()).is_err() {
        return true;
    }
    replies.enqueue_ipv4(
        reply_header(ip, ip.bytes()[1] & 0xfc),
        ip.payload().len(),
        packet.interface_index,
        now,
        |payload| {
            payload[4..].copy_from_slice(&ip.payload()[4..]);
            icmpv4::emit(payload, 0, 0).is_ok()
        },
    )
}

pub(super) fn unreachable(
    packet: Ipv4Ingress<'_>,
    link_group: bool,
    code: u8,
    errors: &mut ErrorBudget,
    replies: &mut RouteReplies,
    now: Instant,
) -> bool {
    let ip = packet.ip;
    if packet.multicast
        || link_group
        || ip.source().octets()[0] == 0
        || !errors.spend(now.total_millis().max(0) as u64)
    {
        return true;
    }
    let quote = &ip.bytes()[..ip.bytes().len().min(548)];
    replies.enqueue_ipv4(
        reply_header(ip, 0),
        8 + quote.len(),
        packet.interface_index,
        now,
        |payload| icmpv4::emit_error(payload, 3, code, 0, quote, &[]).is_ok(),
    )
}

fn reply_header(ip: Ipv4Packet<'_>, dscp_ecn: u8) -> Ipv4Header<'static> {
    Ipv4Header {
        source: ip.destination(),
        destination: ip.source(),
        protocol: 1,
        hop_limit: DEFAULT_IPV4_HOP_LIMIT,
        dscp_ecn,
        identification: 0,
        fragment: 0,
        options: &[],
    }
}
