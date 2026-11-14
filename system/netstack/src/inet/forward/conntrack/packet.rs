//! 报文字段到双向连接键的唯一解析。队列可提前计算散列，跟踪仍验证完整传输语义。

use super::{FlowHash, ReceiveFlow, tcp};
use core::{hash::BuildHasher, net::IpAddr, num::NonZeroU16};
use device_model::network::{ReceiveChecksum, ReceivePacket};
use packet_wire::{
    icmpv4::{self, Icmpv4Packet},
    icmpv6::{self, Icmpv6Packet},
    ipv4::{Ipv4Packet, Ipv4Quote},
    ipv6::{Ipv6Datagram, Ipv6Packet, Ipv6Quote},
    tcp::TcpPacket,
    udp::UdpPacket,
};
use router_protocol::firewall::{TransportMatch, TransportProtocol};

pub(super) type Endpoint = (IpAddr, u16);

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) enum Protocol {
    Tcp,
    Udp,
    Echo,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(in crate::inet::forward) struct Key {
    pub(super) endpoints: [Endpoint; 2],
    pub(super) protocol: Protocol,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::inet::forward) struct Datagram {
    pub(super) key: Key,
    pub(super) reversed: bool,
}

impl Datagram {
    fn transport_ports(
        protocol: Protocol,
        source: IpAddr,
        destination: IpAddr,
        bytes: &[u8],
    ) -> Option<Self> {
        let &[a, b, c, d] = bytes.first_chunk::<4>()?;
        Self::ports(
            protocol,
            (source, u16::from_be_bytes([a, b])),
            (destination, u16::from_be_bytes([c, d])),
        )
    }

    pub(super) fn ports(
        protocol: Protocol,
        source: Endpoint,
        destination: Endpoint,
    ) -> Option<Self> {
        if source == destination || source.1 == 0 || destination.1 == 0 {
            return None;
        }
        let reversed = source > destination;
        let endpoints = if reversed {
            [destination, source]
        } else {
            [source, destination]
        };
        Some(Self {
            key: Key {
                endpoints,
                protocol,
            },
            reversed,
        })
    }

    pub(super) fn echo(
        source: IpAddr,
        destination: IpAddr,
        identifier: u16,
        reply: bool,
    ) -> Option<Self> {
        if source == destination {
            return None;
        }
        // Echo 发起者由消息类型确定。两端并发使用同一个 ID 时仍是两条流。
        let (origin, peer) = if reply {
            (destination, source)
        } else {
            (source, destination)
        };
        Some(Self {
            key: Key {
                endpoints: [(origin, identifier), (peer, 0)],
                protocol: Protocol::Echo,
            },
            reversed: reply,
        })
    }

    fn quoted(source: IpAddr, destination: IpAddr, protocol: u8, bytes: &[u8]) -> Option<Packet> {
        if !trackable_addresses(source, destination) {
            return None;
        }
        match (source.is_ipv6(), protocol) {
            (_, 6) => {
                let [a, b, c, d, e, f, g, h] = *bytes.first_chunk::<8>()?;
                let datagram = Self::ports(
                    Protocol::Tcp,
                    (source, u16::from_be_bytes([a, b])),
                    (destination, u16::from_be_bytes([c, d])),
                )?;
                Some(Packet::Error(
                    datagram,
                    Some(u32::from_be_bytes([e, f, g, h])),
                ))
            }
            (_, 17) => {
                let udp = UdpPacket::parse_quote(bytes, source.is_ipv6()).ok()?;
                Self::ports(
                    Protocol::Udp,
                    (source, udp.source_port()),
                    (destination, udp.destination_port()),
                )
                .map(|datagram| Packet::Error(datagram, None))
            }
            (false, 1) => {
                let icmpv4::Message::Echo {
                    reply, identifier, ..
                } = Icmpv4Packet::parse_header(bytes).ok()?.message()
                else {
                    return None;
                };
                Self::echo(source, destination, identifier, reply)
                    .map(|datagram| Packet::Error(datagram, None))
            }
            (true, 58) => {
                let icmpv6::Message::Echo {
                    reply, identifier, ..
                } = Icmpv6Packet::parse_header(bytes).ok()?.message()
                else {
                    return None;
                };
                Self::echo(source, destination, identifier, reply)
                    .map(|datagram| Packet::Error(datagram, None))
            }
            _ => None,
        }
    }
}

pub(super) fn trackable_addresses(source: IpAddr, destination: IpAddr) -> bool {
    [source, destination]
        .into_iter()
        .all(|address| !matches!(address, IpAddr::V6(address) if address.is_unicast_link_local()))
}

pub(in crate::inet::forward) enum Packet {
    Data(Datagram),
    Tcp(Datagram, tcp::Segment),
    Error(Datagram, Option<u32>),
}

/// 只生成可直接定位的 TCP/UDP 结构匹配。采用裁决前须核对同一报文的传输层校验和。
pub(in crate::inet) fn prepare_transport(packet: ReceivePacket<'_>) -> Option<TransportMatch> {
    let (source, destination, protocol, bytes) = packet.direct_transport()?;
    match protocol {
        6 => Packet::tcp(source, destination, bytes, true, None),
        17 => Packet::udp(source, destination, bytes, true),
        _ => None,
    }?
    .transport()
}

impl Packet {
    /// 用线上报文的真实目的端口匹配。ICMP 引用不作为外层报文的传输条件。
    pub(in crate::inet::forward) fn transport(&self) -> Option<TransportMatch> {
        let datagram = match self {
            Self::Data(datagram) | Self::Tcp(datagram, _) => datagram,
            Self::Error(..) => return None,
        };
        let protocol = match datagram.key.protocol {
            Protocol::Tcp => TransportProtocol::Tcp,
            Protocol::Udp => TransportProtocol::Udp,
            Protocol::Echo => return None,
        };
        Some(TransportMatch {
            protocol,
            destination_port: Some(NonZeroU16::new(
                datagram.key.endpoints[usize::from(!datagram.reversed)].1,
            )?),
        })
    }

    pub(in crate::inet::forward) fn ipv4(
        ip: Ipv4Packet<'_>,
        checksum: ReceiveChecksum,
        receive: Option<ReceiveFlow>,
    ) -> Option<Self> {
        if checksum == ReceiveChecksum::Invalid {
            return None;
        }
        let checksum_valid = checksum.verified();
        if ip.fragment_offset() != 0 || ip.more_fragments() {
            return None;
        }
        let source = ip.source().into();
        let destination = ip.destination().into();
        match ip.protocol() {
            6 => Self::tcp(source, destination, ip.payload(), checksum_valid, receive),
            17 => Self::udp(source, destination, ip.payload(), checksum_valid),
            1 => {
                let icmp = if checksum_valid {
                    Icmpv4Packet::parse_header(ip.payload())
                } else {
                    Icmpv4Packet::parse(ip.payload())
                }
                .ok()?;
                match icmp.message() {
                    icmpv4::Message::Echo {
                        reply, identifier, ..
                    } => Datagram::echo(source, destination, identifier, reply).map(Self::Data),
                    icmpv4::Message::Error { body, .. }
                        if matches!(
                            (icmp.message_type(), icmp.code()),
                            (3, 0..=15) | (11, 0..=1) | (12, 0..=2)
                        ) =>
                    {
                        let quote = Ipv4Quote::parse(body.quoted_bytes().ok()?).ok()?;
                        // 外层目的必须是被引用方向的发送端，不能把已知 tuple 当作跨主机放行凭据。
                        if quote.source() != ip.destination() {
                            return None;
                        }
                        Datagram::quoted(
                            quote.source().into(),
                            quote.destination().into(),
                            quote.protocol(),
                            quote.upper_layer()?,
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(in crate::inet::forward) fn ipv6(
        ip: Ipv6Datagram<'_>,
        checksum: ReceiveChecksum,
        receive: Option<ReceiveFlow>,
    ) -> Option<Self> {
        if checksum == ReceiveChecksum::Invalid {
            return None;
        }
        let checksum_valid = checksum.verified();
        let source = ip.source().into();
        let destination = ip.destination().into();
        if ip.bytes()[6] == 6 {
            return Self::tcp(source, destination, ip.payload(), checksum_valid, receive);
        }
        let packet = Ipv6Packet::parse(ip.bytes()).ok()?;
        let bytes = packet.upper_layer()?;
        match packet.upper_protocol() {
            6 => Self::tcp(source, destination, bytes, checksum_valid, None),
            17 => Self::udp(source, destination, bytes, checksum_valid),
            58 => {
                let icmp = if checksum_valid {
                    Icmpv6Packet::parse_header(bytes)
                } else {
                    Icmpv6Packet::parse(bytes, ip.source(), ip.destination())
                }
                .ok()?;
                match icmp.message() {
                    icmpv6::Message::Echo {
                        reply, identifier, ..
                    } => Datagram::echo(source, destination, identifier, reply).map(Self::Data),
                    icmpv6::Message::Error { body, .. }
                        if matches!(
                            (icmp.message_type(), icmp.code()),
                            (1, 0..=6) | (2, 0) | (3, 0..=1) | (4, 0..=2)
                        ) =>
                    {
                        let quote = Ipv6Quote::parse(body.quoted_bytes().ok()?).ok()?;
                        if quote.source() != ip.destination() {
                            return None;
                        }
                        Datagram::quoted(
                            quote.source().into(),
                            quote.destination().into(),
                            quote.upper_protocol(),
                            quote.upper_layer()?,
                        )
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn tcp(
        source: IpAddr,
        destination: IpAddr,
        bytes: &[u8],
        checksum_valid: bool,
        receive: Option<ReceiveFlow>,
    ) -> Option<Self> {
        let segment = if checksum_valid {
            match receive.and_then(ReceiveFlow::prepared_tcp) {
                Some(segment) => segment?,
                None => tcp::Segment::prepare(bytes)?,
            }
        } else {
            tcp::Segment::capture(TcpPacket::parse(bytes, source, destination).ok()?)?
        };
        let datagram = Datagram::transport_ports(Protocol::Tcp, source, destination, bytes)?;
        Some(Self::Tcp(datagram, segment))
    }

    fn udp(
        source: IpAddr,
        destination: IpAddr,
        bytes: &[u8],
        checksum_valid: bool,
    ) -> Option<Self> {
        let udp = if checksum_valid {
            UdpPacket::parse_header(bytes, source.is_ipv6())
        } else {
            UdpPacket::parse(bytes, source, destination)
        }
        .ok()?;
        Datagram::ports(
            Protocol::Udp,
            (source, udp.source_port()),
            (destination, udp.destination_port()),
        )
        .map(Self::Data)
    }
}

impl FlowHash {
    /// 散列直接承载的 TCP/UDP 原始元组，并借出其传输层范围供后续检查。
    pub(super) fn receive(self, packet: ReceivePacket<'_>) -> Option<(Key, u64, &[u8])> {
        let (source, destination, protocol, bytes) = packet.direct_transport()?;
        let protocol = match protocol {
            6 => Protocol::Tcp,
            17 => Protocol::Udp,
            _ => return None,
        };
        let key = Datagram::transport_ports(protocol, source, destination, bytes)?.key;
        Some((key, self.hash_one(key), bytes))
    }
}
