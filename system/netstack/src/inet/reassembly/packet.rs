//! 分片格式与完整包归一化；队列、预算、寿命属于父 owner。
use super::{First, Key};
use core::net::{IpAddr, Ipv4Addr};
use packet_wire::{ethernet::EthernetFrame, ipv4::Ipv4Packet, ipv6::Ipv6Packet};
pub(super) struct Fragment<'a> {
    pub(super) key: Key,
    pub(super) bytes: &'a [u8],
    pub(super) payload: &'a [u8],
    pub(super) offset: usize,
    pub(super) more: bool,
    pub(super) header: usize,
    pub(super) data: usize,
    pub(super) next_field: Option<usize>,
    pub(super) interface: u32,
    pub(super) ecn: u8,
}
pub(super) enum Inspected<'a> {
    Pass,
    Drop,
    Fragment(Fragment<'a>),
    Problem(&'a [u8], (u8, u8, u32)),
}
pub(super) fn inspect(
    frame: &[u8],
    link: Option<([u8; 6], usize)>,
    interface: u32,
    admits: impl FnOnce(IpAddr, IpAddr) -> bool,
) -> Inspected<'_> {
    let bytes = if let Some((mac, _)) = link {
        let Ok(link) = EthernetFrame::parse(frame) else {
            return Inspected::Pass;
        };
        if !matches!(link.type_or_length(), 0x0800 | 0x86dd)
            || link.source() == [0; 6]
            || link.source()[0] & 1 != 0
            || link.destination() != mac && link.destination()[0] & 1 == 0
            || link
                .payload()
                .first()
                .is_none_or(|byte| (byte >> 4 == 4) != (link.type_or_length() == 0x0800))
        {
            return Inspected::Pass;
        }
        link.payload()
    } else {
        frame
    };
    match bytes.first().map(|byte| byte >> 4) {
        Some(4) => {
            if bytes.len() < 20 || bytes[6] & 0x3f == 0 && bytes[7] == 0 {
                return Inspected::Pass;
            }
            let Ok(ip) = Ipv4Packet::parse_header(bytes) else {
                return Inspected::Drop;
            };
            if !admits(ip.source().into(), ip.destination().into()) {
                return Inspected::Pass;
            }
            // 转发片交回原转发入口验证 checksum，本机片才在此承担重组前的完整校验。
            if packet_wire::checksum::checksum(&ip.bytes()[..ip.bytes().len() - ip.payload().len()])
                != 0
            {
                return Inspected::Drop;
            }
            let key = Key {
                source: ip.source().into(),
                destination: ip.destination().into(),
                id: u32::from(ip.identification()),
                protocol: ip.protocol(),
                zone: if ip.source().is_link_local()
                    || ip.destination().is_link_local()
                    || ip.destination().is_multicast()
                {
                    interface
                } else {
                    0
                },
            };
            Inspected::Fragment(Fragment {
                key,
                bytes: ip.bytes(),
                payload: ip.payload(),
                offset: ip.fragment_offset(),
                more: ip.more_fragments(),
                header: ip.bytes().len() - ip.payload().len(),
                data: ip.bytes().len() - ip.payload().len(),
                next_field: None,
                interface,
                ecn: bytes[1] & 3,
            })
        }
        Some(6) => {
            // 普通 TCP/UDP 快路径不扫描扩展链，不分配或复制载荷。
            if bytes.len() < 40 || !matches!(bytes[6], 0 | 43 | 44 | 51 | 60) {
                return Inspected::Pass;
            }
            let Ok(datagram) = packet_wire::ipv6::Ipv6Datagram::parse(bytes) else {
                return Inspected::Drop;
            };
            if let Some((mac, mtu)) = link
                && super::super::ipv6::incoming(frame, mac, mtu, |_| true).is_none()
            {
                return Inspected::Pass;
            }
            if !admits(datagram.source().into(), datagram.destination().into()) {
                return Inspected::Pass;
            }
            let ip = match Ipv6Packet::parse(datagram.bytes()) {
                Ok(ip) => ip,
                Err(error) => {
                    return Inspected::Problem(
                        datagram.bytes(),
                        (
                            4,
                            if error.incomplete_first_fragment {
                                3
                            } else {
                                0
                            },
                            if error.incomplete_first_fragment {
                                0
                            } else {
                                error.offset as u32
                            },
                        ),
                    );
                }
            };
            if !ip.needs_reassembly() {
                return Inspected::Pass;
            }
            if !super::super::ipv6::fragment_extensions(ip) {
                return Inspected::Drop;
            }
            let mut previous = 6;
            for header in ip.extensions() {
                if let Some(fragment) = header.fragment() {
                    let at = header.offset();
                    if fragment.fragment_offset() == 0 {
                        let upper = ip
                            .upper_layer()
                            .unwrap_or_else(|| &ip.bytes()[ip.upper_offset()..]);
                        let required = match ip.upper_protocol() {
                            6 => upper
                                .get(12)
                                .map_or(20, |byte| usize::from(byte >> 4).max(5) * 4),
                            17 | 58 => 8,
                            _ => 0,
                        };
                        if upper.len() < required {
                            return Inspected::Problem(ip.bytes(), (4, 3, 0));
                        }
                    }
                    return Inspected::Fragment(Fragment {
                        key: Key {
                            source: ip.source().into(),
                            destination: ip.destination().into(),
                            id: fragment.identification(),
                            protocol: 0,
                            zone: if ip.source().is_unicast_link_local()
                                || ip.destination().is_unicast_link_local()
                                || ip.destination().is_multicast()
                            {
                                interface
                            } else {
                                0
                            },
                        },
                        bytes: ip.bytes(),
                        payload: &ip.bytes()[at + 8..],
                        offset: fragment.fragment_offset(),
                        more: fragment.more(),
                        header: at,
                        data: at + 8,
                        next_field: Some(previous),
                        interface,
                        ecn: ip.traffic_class() & 3,
                    });
                }
                previous = header.offset();
            }
            Inspected::Pass
        }
        _ => Inspected::Pass,
    }
}
pub(super) fn normalize(bytes: &mut [u8], first: &First, ecn: u8) -> Option<()> {
    // RFC 3168 / Linux ip_frag_ecn_table：Not-ECT 混合非法；有 CE 则向完整包保留拥塞信号。
    let ce = if ecn & 8 != 0 { 3 } else { 0 };
    if let Some(field) = first.next_field {
        bytes[field] = first.quote[first.header];
        let length = u16::try_from(bytes.len().checked_sub(40)?).ok()?;
        bytes[4..6].copy_from_slice(&length.to_be_bytes());
        bytes[1] |= ce << 4;
    } else {
        let length = u16::try_from(bytes.len()).ok()?;
        bytes[2..4].copy_from_slice(&length.to_be_bytes());
        bytes[6..8].fill(0);
        bytes[1] |= ce;
        bytes[10..12].fill(0);
        let checksum = packet_wire::checksum::checksum(&bytes[..first.header]);
        bytes[10..12].copy_from_slice(&checksum.to_be_bytes());
    }
    Some(())
}
/// 无差别错误响应会形成放大器；先排除非单播源/目的及 ICMP 错误，引用必须含首片。
pub(super) fn error_flow(quote: &[u8], reason: (u8, u8, u32)) -> Option<(IpAddr, IpAddr)> {
    match quote.first()? >> 4 {
        4 => {
            if quote.len() < 20 {
                return None;
            }
            let source = Ipv4Addr::from(<[u8; 4]>::try_from(&quote[12..16]).ok()?);
            let destination = Ipv4Addr::from(<[u8; 4]>::try_from(&quote[16..20]).ok()?);
            if source.is_unspecified()
                || source.is_multicast()
                || source.is_broadcast()
                || destination.is_multicast()
                || destination.is_broadcast()
                || quote[6] & 31 != 0
                || quote[7] != 0
            {
                return None;
            }
            if quote[9] == 1
                && quote
                    .get(usize::from(quote[0] & 15) * 4)
                    .is_none_or(|kind| matches!(kind, 3..=5 | 11 | 12))
            {
                return None;
            }
            Some((destination.into(), source.into()))
        }
        6 => {
            if quote.len() < 40 {
                return None;
            }
            let source = core::net::Ipv6Addr::from(<[u8; 16]>::try_from(&quote[8..24]).ok()?);
            let destination = core::net::Ipv6Addr::from(<[u8; 16]>::try_from(&quote[24..40]).ok()?);
            if source.is_unspecified()
                || source.is_multicast()
                || source.to_ipv4_mapped().is_some()
                || destination.is_multicast()
            {
                return None;
            }
            match packet_wire::ipv6::quoted_upper_prefix(quote) {
                Ok(Some((58, payload)))
                    if payload
                        .first()
                        .is_none_or(|kind| *kind < 128 || *kind == 137) =>
                {
                    return None;
                }
                Ok(Some(_)) => {}
                Err(_) if reason.0 == 4 && reason.1 == 3 => {}
                _ => return None,
            }
            Some((destination.into(), source.into()))
        }
        _ => None,
    }
}
