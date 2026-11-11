//! 本机 ICMPv6 错误发送策略；不持 socket、邻居或载荷副本。
use super::Packet;
use packet_wire::ipv6::Ipv6Packet;

/// UDP owner 已验证 checksum 且确认无监听者；非法/多播源与无回程路由不能生成回复。
pub(in crate::inet) fn port_unreachable<'a>(
    budget: &mut super::super::route_reply::ErrorBudget,
    ip: Ipv6Packet<'a>,
    now: u64,
) -> Option<Packet<'a>> {
    if ip.source().is_unspecified()
        || ip.source().is_multicast()
        || ip.source().to_ipv4_mapped().is_some()
        || ip.destination().is_multicast()
    {
        return None;
    }
    if !budget.spend(now) {
        return None;
    }
    Some(Packet::Unreachable {
        source: ip.destination(),
        destination: ip.source(),
        destination_mac: [0; 6],
        hop_limit: super::DEFAULT_HOP_LIMIT,
        quote: &ip.bytes()[..ip.bytes().len().min(1232)],
    })
}
