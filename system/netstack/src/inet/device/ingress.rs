//! 批内格式检查产生的短寿命事实；不拥有协议状态，也不能跳过引擎的 checksum/接收准入。

use core::net::Ipv4Addr;
use packet_wire::batch::{NetworkPacket, ParsedFrame};

#[derive(Clone, Copy)]
pub(in crate::inet) enum IngressFacts {
    Ipv4 {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
    },
    Ipv6,
}
impl IngressFacts {
    pub(super) fn addresses(self) -> Option<(Ipv4Addr, Ipv4Addr)> {
        match self {
            Self::Ipv4 {
                source,
                destination,
                ..
            } => Some((source, destination)),
            Self::Ipv6 => None,
        }
    }
    pub(super) fn from_ipv4(ip: packet_wire::ipv4::Ipv4Packet<'_>) -> Self {
        Self::Ipv4 {
            source: ip.source(),
            destination: ip.destination(),
            protocol: ip.protocol(),
        }
    }
    pub(super) fn from_frame(frame: ParsedFrame<'_>) -> Option<Self> {
        // 当前主机协议引擎未接入 VLAN；内层事实不能替它绕过链路接收策略。
        match (frame.ethernet.type_or_length(), frame.network) {
            (0x0800, NetworkPacket::Ipv4(ip)) => Some(Self::from_ipv4(ip)),
            (0x86dd, NetworkPacket::Ipv6(_)) => Some(Self::Ipv6),
            _ => None,
        }
    }
}
