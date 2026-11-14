//! 本机 ICMP 错误准入、引用解析和真实发送端点分派。
use super::*;
use core::net::IpAddr;
use network_protocol::{
    IcmpErrorExtension, InternetError, InternetErrorOrigin, InternetPacketInfo,
};
use packet_wire::{
    icmp::ErrorBody,
    icmpv4, icmpv6,
    ipv4::Ipv4Quote,
    ipv6::{Ipv6Packet, Ipv6Quote},
};

/// 借用同一条已验证引用。UDP、RAW 与 TCP 使用相同的 IP 事实。
pub(super) struct Report<'a> {
    pub(super) source: IpAddr,
    pub(super) destination: IpAddr,
    pub(super) protocol: u8,
    pub(super) transport: &'a [u8],
    pub(super) transport_length: Option<usize>,
    pub(super) quote: &'a [u8],
    pub(super) interface: u32,
    pub(super) hard: bool,
    status: SocketError,
    offender: IpAddr,
    kind: u8,
    code: u8,
    packet_info: InternetPacketInfo,
    multipart: Option<(IcmpErrorExtension, &'a [u8])>,
}
impl<'a> Report<'a> {
    pub(super) fn too_big(&self) -> bool {
        self.status == SocketError::MessageTooLarge
    }

    /// ICMP errors suppressed by default for synchronized TCP-AO connections.
    pub(super) fn ao_hard_error(&self) -> bool {
        matches!(
            (self.source.is_ipv6(), self.kind, self.code),
            (false, 3, 2..=4) | (true, 1, 1 | 4)
        )
    }

    pub(super) fn record(
        &self,
        port: u16,
        info: u32,
        (visible, skip): (&'a [u8], usize),
        present: impl Fn(InetAddress) -> InetAddress,
    ) -> (InternetError, &'a [u8]) {
        let scoped = |ip: IpAddr, port| {
            let mut address = InetAddress::new(ip, port);
            if let InetAddress::V6(value) = &mut address
                && value.ip().is_unicast_link_local()
            {
                value.set_scope_id(self.interface);
            }
            present(address)
        };
        // UDP 和 RAW 剥离的头长度不同。应用收到填充与扩展原字节，偏移以实际交付起点为准。
        let (extension, bytes) = self
            .multipart
            .and_then(|(mut extension, wire)| {
                extension.offset = extension.offset.checked_sub(u16::try_from(skip).ok()?)?;
                Some((Some(extension), wire.get(skip..)?))
            })
            .unwrap_or((None, visible));
        (
            InternetError {
                error: self.status,
                origin: if self.source.is_ipv4() {
                    InternetErrorOrigin::Icmpv4
                } else {
                    InternetErrorOrigin::Icmpv6
                },
                kind: self.kind,
                code: self.code,
                info,
                destination: scoped(self.destination, port),
                offender: Some(scoped(self.offender, 0)),
                extension,
                packet_info: Some(self.packet_info),
            },
            bytes,
        )
    }

    pub(super) fn transport_offset(&self) -> usize {
        self.quote.len() - self.transport.len()
    }
}

fn multipart(body: ErrorBody<'_>) -> Option<(IcmpErrorExtension, &[u8])> {
    let offset = body.quoted_bytes().ok()?.len();
    let wire = body.bytes();
    if wire.len() - offset < 4 {
        return None;
    }
    let extension = IcmpErrorExtension {
        offset: u16::try_from(offset).ok()?,
        invalid: body.extensions().is_err(),
    };
    Some((extension, wire))
}

fn status(ipv6: bool, kind: u8, code: u8) -> Option<(SocketError, bool)> {
    use SocketError::*;
    Some(match (ipv6, kind, code) {
        (false, 3, 0 | 11) | (true, 1, 0) => (NetworkUnreachable, false),
        (false, 3, 6 | 9) => (NetworkUnreachable, true),
        (false, 3, 1 | 12) | (true, 1, 2 | 3) => (HostUnreachable, false),
        (false, 3, 10 | 13..=15) => (HostUnreachable, true),
        (false, 3, 2) => (ProtocolOptionUnavailable, true),
        (false, 3, 3) | (true, 1, 4) => (ConnectionRefused, true),
        (false, 3, 4) | (true, 2, 0) => (MessageTooLarge, true),
        (false, 3, 5) => (OperationNotSupported, false),
        (false, 3, 7) => (HostDown, true),
        (false, 3, 8) => (NetworkIsolated, true),
        (true, 1, 1 | 5 | 6) => (PermissionDenied, true),
        (false, 11, 0 | 1) | (true, 3, 0 | 1) => (HostUnreachable, false),
        (false, 12, 0..=2) | (true, 4, 0..=2) => (ProtocolError, true),
        _ => return None,
    })
}
type ErrorEndpoints<'a> = (
    &'a mut FallibleMap<u64, super::udp::EndpointState>,
    &'a mut FallibleMap<usize, super::tcp::TcpEndpointState>,
    &'a mut FallibleMap<u64, super::raw::RawEndpointState>,
);

/// 校验 IPv4 外层及引用，再由实际发送端点确认路由与错误。
pub(super) fn receive_ipv4(
    paths: &mut super::path_mtu::PathMtu,
    packet: crate::inet::ingress::Ipv4Ingress<'_>,
    endpoints: ErrorEndpoints<'_>,
    (routing, rules): (
        &super::route::RoutingTable,
        &[router_protocol::policy_routing::Rule],
    ),
    facts: super::route::Ipv4Interfaces<'_>,
    now: Instant,
) {
    if packet.multicast || !matches!(packet.ip.payload().first(), Some(3 | 11 | 12)) {
        return;
    }
    let Ok(icmp) = icmpv4::Icmpv4Packet::parse(packet.ip.payload()) else {
        return;
    };
    let kind = icmp.message_type();
    let code = icmp.code();
    let Some((status, hard)) = status(false, kind, code) else {
        return;
    };
    let icmpv4::Message::Error { parameter, body } = icmp.message() else {
        return;
    };
    let Ok(quote) = body.quoted_bytes().and_then(Ipv4Quote::parse) else {
        return;
    };
    let source = quote.source();
    let destination = quote.destination();
    let interface = packet.interface_index;
    if source != packet.ip.destination()
        || status == SocketError::MessageTooLarge && !quote.dont_fragment()
        || !facts
            .iter()
            .any(|fact| fact.up && fact.address == Some(source))
        || destination.is_unspecified()
        || destination.is_multicast()
        || destination.is_broadcast()
    {
        return;
    }
    let Some(bytes) = quote
        .upper_layer()
        .filter(|bytes| bytes.len() >= quote.upper_length().min(8))
    else {
        return;
    };
    let report = Report {
        source: source.into(),
        destination: destination.into(),
        protocol: quote.protocol(),
        transport: bytes,
        transport_length: (!quote.more_fragments()).then_some(quote.upper_length()),
        quote: quote.bytes(),
        interface,
        hard,
        status,
        offender: packet.ip.source().into(),
        kind,
        code,
        packet_info: InternetPacketInfo::Ipv4 {
            interface_index: interface,
            specific_address: packet.facts.specific_destination.unwrap_or(source),
            destination: source,
        },
        multipart: multipart(body),
    };
    let path = super::path_mtu::Path {
        interface,
        source: report.source,
        destination: report.destination,
    };
    let now = now.total_millis().max(0) as u64;
    let accept = |(bound, mark), mode| {
        let route = routing
            .select_output(source, destination, bound, (mark, rules), facts)
            .ok()?;
        if route.kind != network_protocol::RouteKind::Unicast || route.interface_index != interface
        {
            return None;
        }
        if !report.too_big() {
            return Some(if kind == 12 { parameter >> 24 } else { 0 });
        }
        let reported = parameter as u16;
        let mtu = if reported == 0 {
            // RFC 1191 §5/§7 按原长度选择固定档位，重复引用不逐次压低路径。
            [
                65535, 32000, 17914, 8166, 4352, 2002, 1492, 1006, 508, 296, 68,
            ]
            .into_iter()
            .find(|mtu| *mtu < quote.original_length())
            .unwrap_or(68)
        } else {
            usize::from(reported)
        };
        if mtu < 68 || mtu >= route.mtu || mtu >= quote.original_length() {
            return None;
        }
        if mode < 4 {
            let _ = paths.learn(path, mtu as u32, now);
        }
        Some(mtu as u32)
    };
    deliver(endpoints, &report, accept);
}

/// 校验 IPv6 外层 checksum、错误类型及原包引用。传输 owner 再核对端口、设备和序号。
pub(super) fn receive_ipv6(
    paths: &mut super::path_mtu::PathMtu,
    ip: Ipv6Packet<'_>,
    endpoints: ErrorEndpoints<'_>,
    interface: u32,
    link_mtu: usize,
    now: u64,
) {
    let Some(icmp) = ip
        .upper_layer()
        .and_then(|bytes| icmpv6::Icmpv6Packet::parse(bytes, ip.source(), ip.destination()).ok())
    else {
        return;
    };
    let kind = icmp.message_type();
    let code = icmp.code();
    let Some((status, hard)) = status(true, kind, code) else {
        return;
    };
    let icmpv6::Message::Error { parameter, body } = icmp.message() else {
        return;
    };
    let Some(quote) = body
        .quoted_bytes()
        .ok()
        .and_then(|bytes| Ipv6Quote::parse(bytes).ok())
    else {
        return;
    };
    if quote.source() != ip.destination() {
        return;
    }
    let Some(transport) = quote.upper_layer() else {
        return;
    };
    let report = Report {
        source: quote.source().into(),
        destination: quote.destination().into(),
        protocol: quote.upper_protocol(),
        transport,
        transport_length: quote.upper_length(),
        quote: quote.bytes(),
        interface,
        hard,
        status,
        offender: ip.source().into(),
        kind,
        code,
        packet_info: InternetPacketInfo::Ipv6 {
            interface_index: interface,
            destination: ip.destination(),
        },
        multipart: multipart(body),
    };
    if report.too_big() && (parameter < 1280 || parameter as usize >= quote.original_length()) {
        return;
    }
    deliver(endpoints, &report, |_, _| {
        if report.too_big() && (parameter as usize) < link_mtu {
            let _ = paths.learn(
                super::path_mtu::Path {
                    interface,
                    source: report.source,
                    destination: report.destination,
                },
                parameter,
                now,
            );
        }
        Some(if report.too_big() || kind == 4 {
            parameter
        } else {
            0
        })
    });
}

fn deliver(
    (udp, tcp, raw): ErrorEndpoints<'_>,
    report: &Report<'_>,
    mut accept: impl FnMut((Option<u32>, u32), u8) -> Option<u32>,
) {
    match report.protocol {
        6 if report.too_big() || report.ao_hard_error() => {
            if super::tcp::receive_icmp_error(tcp, report, |egress| accept(egress, 1).is_some()) {
                return;
            }
        }
        17 => super::udp::receive_icmp_error(udp, report, &mut accept),
        _ => {}
    }
    super::raw::receive_icmp_error(raw, report, accept);
}
