//! 本机回复与链路控制报文装配；所有头和 checksum 都经 packet-wire，不持有 NIC 或协议状态。
use core::net::Ipv6Addr;
use packet_wire::{
    PacketError,
    ethernet::{EthernetFrame, EthernetHeader},
    icmpv6,
    ipv6::{
        Ipv6Datagram, Ipv6Header, Ipv6Packet,
        options::{self, OptionData},
    },
    mld,
    multicast::{GroupRecordHeader, RecordType},
    ndisc::{NdMessage, options::OptionValue},
};
pub(super) const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
pub(super) const ALL_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 2);
const MLD_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x16);
use router_protocol::configuration::rdnss_size;

const LINK_IP: usize = 40;

pub(super) fn multicast_mac(address: Ipv6Addr) -> [u8; 6] {
    let bytes = address.octets();
    [0x33, 0x33, bytes[12], bytes[13], bytes[14], bytes[15]]
}
#[derive(Clone, Copy)]
pub(in crate::inet) enum Packet<'a> {
    Buffered {
        frame: &'a [u8],
        destination_mac: [u8; 6],
    },
    RouterSolicitation {
        source: Ipv6Addr,
    },
    RouterAdvertisement {
        source: Ipv6Addr,
        lifetime: u16,
        mtu: u32,
        prefixes: &'a [super::advertisement::PrefixInformation],
        withdrawn: &'a [Ipv6Addr],
        dns: &'a [[u8; 16]],
        dns_lifetime: u32,
        withdrawn_dns: &'a [[u8; 16]],
    },
    Solicitation {
        source: Ipv6Addr,
        target: Ipv6Addr,
        destination: Ipv6Addr,
        destination_mac: [u8; 6],
    },
    EchoReply {
        source: Ipv6Addr,
        destination: Ipv6Addr,
        destination_mac: [u8; 6],
        traffic_class: u8,
        hop_limit: u8,
        // 仅借当前 RX token 的 identifier/sequence/data，不能跨 consume 或交给队列。
        body: &'a [u8],
    },
    Unreachable {
        source: Ipv6Addr,
        destination: Ipv6Addr,
        destination_mac: [u8; 6],
        hop_limit: u8,
        quote: &'a [u8],
    },
    Probe {
        target: Ipv6Addr,
        nonce: [u8; 6],
    },
    Advertisement {
        source: Ipv6Addr,
        destination: Ipv6Addr,
        destination_mac: [u8; 6],
        flags: u32,
    },
    ReportV1 {
        source: Ipv6Addr,
        group: Ipv6Addr,
        done: bool,
    },
    ReportV2 {
        source: Ipv6Addr,
        group: Ipv6Addr,
        kind: RecordType,
        sources: &'a [Ipv6Addr],
    },
}
impl Packet<'_> {
    /// 对已验证本地目的的 Echo 请求构造借用回复；单播源由调用者固定，多播源属于接收接口。
    pub(in crate::inet) fn echo_reply(ip: Ipv6Packet<'_>, source: Ipv6Addr) -> Option<Packet<'_>> {
        if ip.upper_protocol() != 58 || ip.source().is_unspecified() || ip.source().is_multicast() {
            return None;
        }
        let icmp =
            icmpv6::Icmpv6Packet::parse(ip.upper_layer()?, ip.source(), ip.destination()).ok()?;
        if icmp.message_type() != 128 || icmp.code() != 0 {
            return None;
        }
        Some(Packet::EchoReply {
            source,
            destination: ip.source(),
            destination_mac: [0; 6],
            traffic_class: ip.traffic_class(),
            hop_limit: super::DEFAULT_HOP_LIMIT,
            body: icmp.body(),
        })
    }
    /// 仅本机 IP 回复参与 FIB 选路；ND/MLD 的目的属于原链路，不沿默认路由转送。
    pub(in crate::inet) fn reply_flow(self) -> Option<(Ipv6Addr, Ipv6Addr)> {
        match self {
            Self::EchoReply {
                source,
                destination,
                ..
            }
            | Self::Unreachable {
                source,
                destination,
                ..
            } => Some((source, destination)),
            _ => None,
        }
    }
    pub(super) fn unicast_destination(self) -> Option<Ipv6Addr> {
        match self {
            Self::EchoReply { destination, .. }
            | Self::Advertisement { destination, .. }
            | Self::Unreachable { destination, .. }
                if !destination.is_multicast() =>
            {
                Some(destination)
            }
            _ => None,
        }
    }
    pub(super) fn with_hop_limit(mut self, value: u8) -> Self {
        if let Self::EchoReply { hop_limit, .. } | Self::Unreachable { hop_limit, .. } = &mut self {
            *hop_limit = value;
        }
        self
    }
    pub(super) fn resolved(mut self, mac: [u8; 6]) -> Self {
        match &mut self {
            Self::EchoReply {
                destination_mac, ..
            }
            | Self::Advertisement {
                destination_mac, ..
            }
            | Self::Unreachable {
                destination_mac, ..
            } => *destination_mac = mac,
            _ => unreachable!("only an unresolved unicast reply requests neighbor lookup"),
        }
        self
    }
    pub(in crate::inet) fn length(self) -> usize {
        if let Self::Buffered { frame, .. } = self {
            return frame.len();
        }
        14 + LINK_IP
            + match self {
                Self::Buffered { .. } => unreachable!("buffered frame length already returned"),
                Self::RouterSolicitation { source } => {
                    if source.is_unspecified() {
                        8
                    } else {
                        16
                    }
                }
                Self::Solicitation { .. } => 32,
                Self::RouterAdvertisement {
                    prefixes,
                    withdrawn,
                    dns,
                    withdrawn_dns,
                    ..
                } => {
                    32 + 32 * (prefixes.len() + withdrawn.len())
                        + rdnss_size(dns.len())
                        + rdnss_size(withdrawn_dns.len())
                }
                Self::EchoReply { body, .. } => 4 + body.len(),
                Self::Unreachable { quote, .. } => 8 + quote.len(),
                Self::Probe { .. } | Self::Advertisement { .. } => 32,
                Self::ReportV1 { .. } => 8 + 24,
                Self::ReportV2 { sources, .. } => 8 + 8 + 20 + sources.len() * 16,
            }
    }
    /// 写入恰好的最终帧；调用者已预留相同长度，失败不允许提交 NIC。
    pub(in crate::inet) fn emit(self, frame: &mut [u8], mac: [u8; 6]) -> Result<(), PacketError> {
        if frame.len() < 14 || frame.len() != self.length() {
            return Err(PacketError::Capacity);
        }
        if let Self::Buffered {
            frame: pending,
            destination_mac,
        } = self
        {
            frame.copy_from_slice(pending);
            frame[..6].copy_from_slice(&destination_mac);
            return Ok(());
        }
        let destination = self.emit_ip(&mut frame[14..], mac)?;
        EthernetHeader {
            destination,
            source: mac,
            type_or_length: 0x86dd,
            tags: &[],
        }
        .emit(frame)?;
        Ok(())
    }
    /// 在最终 IP-medium 窗口直接编码；回环和 Ethernet 共用同一正文/校验和，不创建中间帧。
    pub(in crate::inet) fn emit_ip(
        self,
        frame: &mut [u8],
        mac: [u8; 6],
    ) -> Result<[u8; 6], PacketError> {
        if frame.len() != self.length() - 14 {
            return Err(PacketError::Capacity);
        }
        if let Self::Buffered {
            frame: pending,
            destination_mac,
        } = self
        {
            frame.copy_from_slice(&pending[14..]);
            return Ok(destination_mac);
        }
        let (source, destination, destination_mac, mld) = match self {
            Self::Buffered { .. } => unreachable!("buffered frame already emitted"),
            Self::Solicitation {
                source,
                target,
                destination,
                destination_mac,
            } => {
                OptionValue::LinkLayerAddress {
                    source: true,
                    address: &mac,
                }
                .emit(&mut frame[LINK_IP + 24..])?;
                NdMessage::NeighborSolicitation { target }.emit(
                    &mut frame[LINK_IP..],
                    source,
                    destination,
                )?;
                (source, destination, destination_mac, false)
            }

            Self::EchoReply {
                source,
                destination,
                destination_mac,
                body,
                ..
            } => {
                frame[LINK_IP + 4..].copy_from_slice(body);
                icmpv6::emit(&mut frame[LINK_IP..], 129, 0, source, destination)?;
                (source, destination, destination_mac, false)
            }
            Self::Unreachable {
                source,
                destination,
                destination_mac,
                quote,
                ..
            } => {
                icmpv6::emit_error(
                    &mut frame[LINK_IP..],
                    1,
                    4,
                    0,
                    (source, destination),
                    quote,
                    &[],
                )?;
                (source, destination, destination_mac, false)
            }
            Self::RouterSolicitation { source } => {
                if !source.is_unspecified() {
                    OptionValue::LinkLayerAddress {
                        source: true,
                        address: &mac,
                    }
                    .emit(&mut frame[LINK_IP + 8..])?;
                }
                NdMessage::RouterSolicitation.emit(&mut frame[LINK_IP..], source, ALL_ROUTERS)?;
                (source, ALL_ROUTERS, multicast_mac(ALL_ROUTERS), false)
            }
            Self::RouterAdvertisement {
                source,
                lifetime,
                mtu,
                prefixes,
                withdrawn,
                dns,
                dns_lifetime,
                withdrawn_dns,
            } => {
                let body = &mut frame[LINK_IP..];
                OptionValue::LinkLayerAddress {
                    source: true,
                    address: &mac,
                }
                .emit(&mut body[16..24])?;
                OptionValue::Mtu(mtu).emit(&mut body[24..32])?;
                let entries = prefixes
                    .iter()
                    .copied()
                    .chain(withdrawn.iter().map(|prefix| (*prefix, 0, 0)));
                let prefix_end = 32 + 32 * (prefixes.len() + withdrawn.len());
                for ((prefix, preferred_lifetime, valid_lifetime), bytes) in
                    entries.zip(body[32..prefix_end].as_chunks_mut::<32>().0)
                {
                    OptionValue::Prefix {
                        length: 64,
                        flags: 0xc0,
                        valid_lifetime,
                        preferred_lifetime,
                        prefix,
                    }
                    .emit(bytes)?;
                }
                let mut offset = prefix_end;
                for (lifetime, servers) in [(dns_lifetime, dns), (0, withdrawn_dns)] {
                    if !servers.is_empty() {
                        offset += OptionValue::RecursiveDnsServers { lifetime, servers }
                            .emit(&mut body[offset..])?;
                    }
                }
                NdMessage::RouterAdvertisement {
                    hop_limit: super::DEFAULT_HOP_LIMIT,
                    flags: 0,
                    lifetime,
                    reachable_time: 0,
                    retransmit_timer: 0,
                }
                .emit(body, source, ALL_NODES)?;
                (source, ALL_NODES, multicast_mac(ALL_NODES), false)
            }
            Self::Probe { target, nonce } => {
                let destination = super::solicited(target);
                OptionValue::Unknown {
                    kind: 14,
                    data: &nonce,
                }
                .emit(&mut frame[LINK_IP + 24..])?;
                NdMessage::NeighborSolicitation { target }.emit(
                    &mut frame[LINK_IP..],
                    Ipv6Addr::UNSPECIFIED,
                    destination,
                )?;
                (
                    Ipv6Addr::UNSPECIFIED,
                    destination,
                    multicast_mac(destination),
                    false,
                )
            }
            Self::Advertisement {
                source,
                destination,
                destination_mac,
                flags,
            } => {
                OptionValue::LinkLayerAddress {
                    source: false,
                    address: &mac,
                }
                .emit(&mut frame[LINK_IP + 24..])?;
                NdMessage::NeighborAdvertisement {
                    flags,
                    target: source,
                }
                .emit(&mut frame[LINK_IP..], source, destination)?;
                (source, destination, destination_mac, false)
            }
            Self::ReportV1 {
                source,
                group,
                done,
            } => {
                let destination = if done { ALL_ROUTERS } else { group };
                mld::emit_report_v1(&mut frame[LINK_IP + 8..], done, group, source, destination)?;
                (source, destination, multicast_mac(destination), true)
            }
            Self::ReportV2 {
                source,
                group,
                kind,
                sources,
            } => {
                let record = &mut frame[LINK_IP + 8 + 8..];
                GroupRecordHeader {
                    record_type: kind,
                    group: group.into(),
                    source_count: u16::try_from(sources.len())
                        .map_err(|_| PacketError::Capacity)?,
                }
                .emit(record)?;
                for (bytes, address) in record[20..].as_chunks_mut::<16>().0.iter_mut().zip(sources)
                {
                    bytes.copy_from_slice(&address.octets());
                }
                mld::emit_report_v2(&mut frame[LINK_IP + 8..], 1, 0, source, MLD_ROUTERS)?;
                (source, MLD_ROUTERS, multicast_mac(MLD_ROUTERS), true)
            }
        };
        if mld {
            options::emit(
                &mut frame[LINK_IP..LINK_IP + 8],
                58,
                &[OptionData {
                    kind: 5,
                    data: &[0, 0],
                    alignment: (2, 0),
                }],
            )?;
        }
        Ipv6Header {
            source,
            destination,
            traffic_class: match self {
                Self::EchoReply { traffic_class, .. } => traffic_class,
                _ => 0,
            },
            flow_label: 0,
            next_header: if mld { 0 } else { 58 },
            hop_limit: match self {
                Self::EchoReply { hop_limit, .. } | Self::Unreachable { hop_limit, .. } => {
                    hop_limit
                }
                _ if mld => 1,
                _ => 255,
            },
        }
        .emit(frame)?;
        Ok(destination_mac)
    }
}
/// 验证以太网 IPv6 的链路及地址准入。已有的 `prepared` 必须借用同一原帧。
pub(in crate::inet) fn incoming<'a>(
    frame: &'a [u8],
    mac: [u8; 6],
    mtu: usize,
    prepared: Option<Ipv6Datagram<'a>>,
    mut accepts: impl FnMut(Ipv6Addr) -> bool,
) -> Option<Ipv6Datagram<'a>> {
    let ethernet = EthernetFrame::parse(frame).ok()?;
    if ethernet.type_or_length() != 0x86dd
        || ethernet.source()[0] & 1 != 0
        || ethernet.source() == [0; 6]
    {
        return None;
    }
    let ip = match prepared {
        Some(ip) => ip,
        None => Ipv6Datagram::parse(ethernet.payload()).ok()?,
    };
    if mtu < 1280
        || ip.bytes().len() > mtu
        || ip.source().is_multicast()
        || ip.source().to_ipv4_mapped().is_some()
        || ip.source().is_loopback()
        || ip.destination().is_loopback()
        || ip.destination().is_multicast()
            && matches!(ip.destination().octets()[1] & 15, 0 | 1 | 15)
        || !accepts(ip.destination())
    {
        return None;
    }
    let destination = if ip.destination().is_multicast() {
        multicast_mac(ip.destination())
    } else {
        mac
    };
    if ethernet.destination() != destination {
        return None;
    }
    Some(ip)
}
/// 以太网与 IP-medium 共用的本地扩展头准入；格式校验不能代替未知选项的 discard 语义。
pub(in crate::inet) fn local_extensions(ip: Ipv6Packet<'_>) -> bool {
    if ip.needs_reassembly() {
        return false;
    }
    ip.extensions()
        .all(|header| header.kind() == 44 || permitted_extension(header))
}
/// 各片的不可分片扩展头在排队前独立验证；首片之后的扩展头在完整重组后统一处理。
pub(in crate::inet) fn fragment_extensions(ip: Ipv6Packet<'_>) -> bool {
    ip.extensions()
        .take_while(|header| header.kind() != 44)
        .all(permitted_extension)
}
fn permitted_extension(header: packet_wire::ipv6::ExtensionHeader<'_>) -> bool {
    // Routing/AH 仍须由其协议 owner 提供语义；未知选项不能绕过 discard 动作。
    if !matches!(header.kind(), 0 | 60) {
        return false;
    }
    let Some(mut options) = header.options() else {
        return false;
    };
    options.all(|option| {
        if option.kind == 5 {
            option.data.len() == 2 && option.offset.is_multiple_of(2)
        } else {
            option.unknown_action() == options::UnknownOptionAction::Skip
        }
    })
}
pub(super) fn router_alert(ip: Ipv6Packet<'_>) -> bool {
    ip.extensions().next().is_some_and(|header| {
        header.kind() == 0
            && header.options().is_some_and(|mut options| {
                options.any(|option| option.kind == 5 && option.data == [0, 0])
            })
    })
}
