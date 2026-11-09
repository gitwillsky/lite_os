//! 批内格式检查产生的短寿命事实；不拥有协议状态，也不能跳过引擎的 checksum/接收准入。

use super::TcpReset;
use core::net::Ipv4Addr;
use packet_wire::batch::{NetworkPacket, ParsedFrame};

#[derive(Clone, Copy)]
pub(in crate::inet) struct IngressFacts {
    pub(super) source: Ipv4Addr,
    pub(super) destination: Ipv4Addr,
    pub(super) reset: Option<TcpReset>,
}
impl IngressFacts {
    pub(super) fn from_ipv4(ip: packet_wire::ipv4::Ipv4Packet<'_>) -> Self {
        Self {
            source: ip.source(),
            destination: ip.destination(),
            reset: (!ip.fragmented() && ip.protocol() == 6)
                .then(|| TcpReset::from_transport(ip.source(), ip.destination(), ip.payload()))
                .flatten(),
        }
    }
    pub(super) fn from_frame(frame: ParsedFrame<'_>) -> Option<Self> {
        // 当前主机协议引擎未接入 VLAN；内层事实不能替它绕过链路接收策略。
        match (frame.ethernet.type_or_length(), frame.network) {
            (0x0800, NetworkPacket::Ipv4(ip)) => Some(Self::from_ipv4(ip)),
            (0x0806, NetworkPacket::Arp(arp))
                if arp.hardware_type() == 1
                    && arp.protocol_type() == 0x0800
                    && arp.sender_hardware().len() == 6
                    && arp.sender_protocol().len() == 4
                    && matches!(arp.operation(), 1 | 2) =>
            {
                Some(Self {
                    source: Ipv4Addr::from(<[u8; 4]>::try_from(arp.sender_protocol()).ok()?),
                    destination: Ipv4Addr::from(<[u8; 4]>::try_from(arp.target_protocol()).ok()?),
                    reset: None,
                })
            }
            _ => None,
        }
    }
}
