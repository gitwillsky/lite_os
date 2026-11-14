//! 同族地址转换只改写最终发送窗口，队列与错误引用保留入站字节。
use super::{
    Forwarding, Ports, Routes, SendResult, TransitPath,
    conntrack::{Datagram, EndpointEdits, TranslationEdit},
    ipv4, ipv6,
};
use crate::inet::port_namespace::PortNamespace;
use core::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    num::{NonZeroU16, NonZeroU32},
};
use network_protocol::{InetAddress, InetProtocol, RouteKind};
use packet_wire::{
    checksum::InternetChecksum,
    icmpv4::{self, Icmpv4Packet},
    icmpv6::{self, Icmpv6Packet},
    ipv4::{Ipv4Packet, Ipv4Quote},
    ipv6::{Ipv6Datagram, Ipv6Packet, Ipv6Quote},
    tcp::{TcpPacket, options::TcpOption},
};
use ::tcp::time::Instant;

/// TCP 与 UDP 当前本机绑定，借自同一个 NET guard。
pub(in crate::inet) type SocketPorts<'a> = (&'a PortNamespace, &'a PortNamespace);

impl Ports<'_> {
    pub(super) fn masquerade_source(
        &self,
        routes: Routes<'_>,
        (source, destination): (IpAddr, IpAddr),
        now: Instant,
    ) -> Result<(NonZeroU32, IpAddr, TransitPath), SendResult> {
        let path = TransitPath::lookup(self, routes, (source, destination), now)?;
        let (interface, address) = match (path, destination) {
            (TransitPath::Ipv4(route), IpAddr::V4(_)) => {
                if route.kind != RouteKind::Unicast {
                    return Err(SendResult::Discard);
                }
                let address = self
                    .facts()
                    .get(route.interface_index)
                    .filter(|facts| facts.up)
                    .and_then(|facts| facts.address)
                    .filter(|address| ipv4::unicast(*address) && !address.is_link_local());
                (route.interface_index, address.map(IpAddr::V4))
            }
            (TransitPath::Ipv6(route), IpAddr::V6(destination)) => {
                let eligible = |address: &Ipv6Addr| {
                    ipv6::unicast(*address) && !address.is_unicast_link_local()
                };
                let address = self
                    .ipv6_facts()
                    .get(route.interface)
                    .filter(|facts| facts.up)
                    .and_then(|facts| {
                        let owner = facts.addresses?;
                        route
                            .preferred_source
                            .filter(|address| eligible(address) && owner.has_address(*address))
                            .or_else(|| owner.source_for(destination).filter(eligible))
                    });
                (route.interface, address.map(IpAddr::V6))
            }
            _ => unreachable!("a datagram has one address family"),
        };
        Ok((
            NonZeroU32::new(interface).expect("FIB selects a real interface"),
            address.ok_or(SendResult::Discard)?,
            path,
        ))
    }

    pub(super) fn socket_source_taken(&self, wire: Datagram) -> bool {
        let Some((protocol, source, port)) = wire.socket_source() else {
            return false;
        };
        let namespace = if protocol == InetProtocol::Tcp {
            self.sockets.0
        } else {
            self.sockets.1
        };
        namespace.covers(source, port)
            && match source {
                IpAddr::V4(address) => self
                    .facts()
                    .iter()
                    .any(|facts| facts.address == Some(address)),
                IpAddr::V6(address) => self.ipv6_facts().iter().any(|facts| {
                    facts
                        .addresses
                        .is_some_and(|owner| owner.has_address(address))
                }),
            }
    }
}

impl Forwarding {
    /// 精确线上元组若已归 NAT，阻止本机连接发出无法区分回包的报文。
    pub(in crate::inet) fn local_tuple_taken(
        &self,
        protocol: InetProtocol,
        endpoints: (InetAddress, InetAddress),
        now: Instant,
    ) -> bool {
        if !self
            .flows
            .translates(endpoints.0.ip().to_canonical().is_ipv6())
        {
            return false;
        }
        self.flows.owns_local_tuple(protocol, endpoints, now)
    }
}
type Addresses = [Option<IpAddr>; 2];
type PortChanges = [Option<(u16, u16)>; 2];

#[derive(Clone, Copy)]
enum AddressEdits {
    Packet(Addresses),
    Quote {
        base: usize,
        endpoints: Addresses,
        // 只有发送错误的地址等于引用的目的时才改写外层源，保留中间路由器的身份。
        from_quoted_destination: bool,
    },
}

impl AddressEdits {
    fn outer(self) -> Addresses {
        match self {
            Self::Packet(endpoints) => endpoints,
            Self::Quote {
                endpoints,
                from_quoted_destination,
                ..
            } => [
                endpoints[1].filter(|_| from_quoted_destination),
                endpoints[0],
            ],
        }
    }
}

#[derive(Clone, Copy)]
/// 从完整入站报文计算的地址及校验和改写，背压时随原始报文保存。
pub(super) struct Rewrite {
    addresses: AddressEdits,
    ports: (usize, [Option<NonZeroU16>; 2]),
    // 以可分片载荷为原点。IPv4 的 TCP checksum 及 ICMP 引用可以落在后续片中。
    checksums: [Option<(usize, u16)>; 3],
    // 未确认的自动转换只在选定出口仍持有该源地址时才可提交。
    pub(super) masquerade_interface: Option<NonZeroU32>,
    // 原始 IPv6 分片以 Fragment header 之后为原点，普通包以固定头之后为原点。
    fragmented_ipv6: bool,
}

impl Rewrite {
    /// 预备最终 TX 所需的改写，认证或报文布局不支持转换时返回 None。
    /// 分片时 `first_fragment` 给出原始首片，`bytes` 是已归一化的完整报文。
    pub(super) fn new(
        bytes: &[u8],
        first_fragment: Option<&[u8]>,
        edit: TranslationEdit,
    ) -> Option<Self> {
        let (addresses, protocol, prefix, offset) = match bytes[0] >> 4 {
            4 => {
                let ip = Ipv4Packet::parse_header(bytes).ok()?;
                let header = bytes.len() - ip.payload().len();
                (
                    (ip.source().into(), ip.destination().into()),
                    ip.protocol(),
                    header,
                    header,
                )
            }
            6 => {
                let ip = Ipv6Packet::parse(bytes).ok()?;
                // Routing/AH 使用额外的目的或认证语义，不能沿普通伪头调整。
                if ip
                    .extensions()
                    .any(|header| matches!(header.kind(), 43 | 51))
                {
                    return None;
                }
                let prefix = match first_fragment {
                    Some(bytes) => fragment(Ipv6Datagram::parse(bytes).ok()?)?.0 - 8,
                    None => 40,
                };
                (
                    (ip.source().into(), ip.destination().into()),
                    ip.upper_protocol(),
                    prefix,
                    ip.upper_offset(),
                )
            }
            _ => return None,
        };
        let fragmented_ipv6 = bytes[0] >> 4 == 6 && first_fragment.is_some();
        let upper = &bytes[offset..];
        let base = offset.checked_sub(prefix)?;
        let mut rewrite = Self {
            addresses: AddressEdits::Packet([None; 2]),
            ports: (0, [None; 2]),
            checksums: [None; 3],
            masquerade_interface: None,
            fragmented_ipv6,
        };
        match edit {
            TranslationEdit::Packet(edits) => {
                if protocol == 6
                    && TcpPacket::parse_header(upper)
                        .ok()?
                        .options()
                        .any(|option| {
                            matches!(
                                option.value,
                                TcpOption::Md5(_) | TcpOption::Authentication { .. }
                            )
                        })
                {
                    // MD5 与 TCP-AO 认证覆盖 IP 伪头，Internet checksum 无法还原其认证。
                    return None;
                }
                let addresses_after = edits.map(|target| target.map(|target| target.address));
                rewrite.addresses = AddressEdits::Packet(addresses_after);
                let ports = rewrite.ports(edits, protocol, upper, base)?;
                rewrite.checksums[0] =
                    transport_word(upper, protocol, addresses, addresses_after, ports)
                        .map(|(at, _, value)| (base + at, value));
            }
            TranslationEdit::Related(quote_edit) => {
                rewrite.error(upper, base, addresses, quote_edit)?
            }
        }
        Some(rewrite)
    }

    fn error(
        &mut self,
        upper: &[u8],
        base: usize,
        addresses: (IpAddr, IpAddr),
        quote_edit: EndpointEdits,
    ) -> Option<()> {
        let (quoted, endpoints, protocol, transport) = if addresses.0.is_ipv4() {
            let icmpv4::Message::Error { body, .. } =
                Icmpv4Packet::parse_header(upper).ok()?.message()
            else {
                return None;
            };
            let quote = Ipv4Quote::parse(body.quoted_bytes().ok()?).ok()?;
            (
                quote.bytes(),
                (quote.source().into(), quote.destination().into()),
                quote.protocol(),
                quote.upper_layer()?,
            )
        } else {
            let icmpv6::Message::Error { body, .. } =
                Icmpv6Packet::parse_header(upper).ok()?.message()
            else {
                return None;
            };
            let quote = Ipv6Quote::parse(body.quoted_bytes().ok()?).ok()?;
            (
                quote.bytes(),
                (quote.source().into(), quote.destination().into()),
                quote.upper_protocol(),
                quote.upper_layer()?,
            )
        };
        let replacements = quote_edit.map(|target| target.map(|target| target.address));
        let quote_base = base + 8;
        self.addresses = AddressEdits::Quote {
            base: quote_base,
            endpoints: replacements,
            from_quoted_destination: addresses.0 == endpoints.1,
        };
        let mut outer = ChecksumUpdate::new(word(upper, 2)?);
        outer.addresses(endpoints, replacements);
        if addresses.0.is_ipv4() {
            let old = word(quoted, 10)?;
            let mut header = ChecksumUpdate::new(old);
            header.addresses(endpoints, replacements);
            let updated = header.sum.checksum();
            outer.replace(&old.to_be_bytes(), &updated.to_be_bytes());
            self.checksums[1] = Some((quote_base + 10, updated));
        }
        let transport_base = quote_base + quoted.len() - transport.len();
        let ports = self.ports(quote_edit, protocol, transport, transport_base)?;
        outer.ports(ports);
        if let Some((at, old, updated)) =
            transport_word(transport, protocol, endpoints, replacements, ports)
        {
            outer.replace(&old.to_be_bytes(), &updated.to_be_bytes());
            self.checksums[2] = Some((transport_base + at, updated));
        }
        if addresses.0.is_ipv6() {
            outer.addresses(addresses, self.addresses.outer());
        }
        self.checksums[0] = Some((base + 2, outer.sum.checksum()));
        Some(())
    }

    fn ports(
        &mut self,
        edits: EndpointEdits,
        protocol: u8,
        upper: &[u8],
        base: usize,
    ) -> Option<PortChanges> {
        let ports = edits.map(|target| target.and_then(|target| target.port));
        let mut changes = [None; 2];
        for (index, port) in ports.into_iter().enumerate() {
            if let Some(port) = port {
                if !matches!(protocol, 6 | 17) {
                    return None;
                }
                changes[index] = Some((word(upper, index * 2)?, port.get()));
            }
        }
        self.ports = (base, ports);
        Some(changes)
    }

    pub(super) fn ipv4(self, ip: Ipv4Packet<'_>) -> (Ipv4Addr, Ipv4Addr) {
        let replace = |edit, original| match edit {
            Some(IpAddr::V4(address)) => address,
            None => original,
            _ => unreachable!("IPv4 NAT was validated"),
        };
        let [source, destination] = self.addresses.outer();
        (
            replace(source, ip.source()),
            replace(destination, ip.destination()),
        )
    }

    pub(super) fn ipv6(self, ip: Ipv6Datagram<'_>) -> (Ipv6Addr, Ipv6Addr) {
        let replace = |edit, original| match edit {
            Some(IpAddr::V6(address)) => address,
            None => original,
            _ => unreachable!("IPv6 NAT was validated"),
        };
        let [source, destination] = self.addresses.outer();
        (
            replace(source, ip.source()),
            replace(destination, ip.destination()),
        )
    }

    pub(super) fn ipv6_payload(self, ip: Ipv6Datagram<'_>) -> Option<(usize, usize)> {
        if self.fragmented_ipv6 {
            fragment(ip)
        } else {
            Some((40, 0))
        }
    }

    /// 当前片中最后一个改写字节之后的位置。其后的载荷可从原 RX 槽复制。
    pub(super) fn payload_prefix_len(self, offset: usize, length: usize) -> usize {
        let end = offset + length;
        let mut prefix = 0;
        self.payload_edits(|at, value| {
            if at < end {
                prefix = prefix.max((at + value.len()).min(end).saturating_sub(offset));
            }
        });
        prefix
    }

    pub(super) fn payload(self, output: &mut [u8], offset: usize) {
        self.payload_edits(|at, value| write_overlap(output, offset, at, value));
    }

    fn payload_edits(self, mut write: impl FnMut(usize, &[u8])) {
        if let AddressEdits::Quote {
            base, endpoints, ..
        } = self.addresses
        {
            for (index, address) in endpoints.into_iter().enumerate() {
                match address {
                    Some(IpAddr::V4(address)) => {
                        write(base + 12 + index * 4, &address.octets())
                    }
                    Some(IpAddr::V6(address)) => {
                        write(base + 8 + index * 16, &address.octets())
                    }
                    None => {}
                }
            }
        }
        let (transport, ports) = self.ports;
        for (index, port) in ports.into_iter().enumerate() {
            if let Some(port) = port {
                write(transport + index * 2, &port.get().to_be_bytes());
            }
        }
        for (at, checksum) in self.checksums.into_iter().flatten() {
            write(at, &checksum.to_be_bytes());
        }
    }
}

fn write_overlap(output: &mut [u8], offset: usize, at: usize, value: &[u8]) {
    let start = at.max(offset);
    let end = (at + value.len()).min(offset + output.len());
    if start < end {
        output[start - offset..end - offset].copy_from_slice(&value[start - at..end - at]);
    }
}

fn fragment(ip: Ipv6Datagram<'_>) -> Option<(usize, usize)> {
    for header in Ipv6Packet::parse(ip.bytes()).ok()?.extensions() {
        if matches!(header.kind(), 43 | 51) {
            return None;
        }
        if let Some(fragment) = header.fragment() {
            return Some((fragment.header_offset() + 8, fragment.fragment_offset()));
        }
    }
    None
}

fn word(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn transport_word(
    upper: &[u8],
    protocol: u8,
    before: (IpAddr, IpAddr),
    after: Addresses,
    ports: PortChanges,
) -> Option<(usize, u16, u16)> {
    let at = match protocol {
        6 => 16,
        17 => 6,
        58 => 2,
        _ => return None,
    };
    let old = word(upper, at)?;
    if protocol == 17 && before.0.is_ipv4() && old == 0 {
        return None;
    }
    let mut update = ChecksumUpdate::new(old);
    update.addresses(before, after);
    update.ports(ports);
    let value = update.sum.checksum();
    Some((
        at,
        old,
        if protocol == 17 && value == 0 {
            u16::MAX
        } else {
            value
        },
    ))
}

struct ChecksumUpdate {
    sum: InternetChecksum,
}
impl ChecksumUpdate {
    fn new(checksum: u16) -> Self {
        let mut sum = InternetChecksum::new();
        sum.add(&(!checksum).to_be_bytes());
        Self { sum }
    }
    fn replace<const N: usize>(&mut self, before: &[u8; N], after: &[u8; N]) {
        // RFC 1624 Eqn. 3：HC' = ~(~HC + ~m + m')。替换字段由完整的 16 位字组成。
        if before != after {
            self.sum.add(&before.map(|byte| !byte));
            self.sum.add(after);
        }
    }
    fn ports(&mut self, changes: PortChanges) {
        for (old, value) in changes.into_iter().flatten() {
            self.replace(&old.to_be_bytes(), &value.to_be_bytes());
        }
    }
    fn addresses(&mut self, before: (IpAddr, IpAddr), after: Addresses) {
        for (before, after) in [before.0, before.1].into_iter().zip(after) {
            if let Some(after) = after {
                self.address(before, after);
            }
        }
    }
    fn address(&mut self, before: IpAddr, after: IpAddr) {
        match (before, after) {
            (IpAddr::V4(before), IpAddr::V4(after)) => {
                self.replace(&before.octets(), &after.octets())
            }
            (IpAddr::V6(before), IpAddr::V6(after)) => {
                self.replace(&before.octets(), &after.octets())
            }
            _ => unreachable!("NAT addresses must use the same family"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_tail_leaves_every_nat_edit_in_the_rendered_prefix() {
        let edit = Rewrite {
            addresses: AddressEdits::Quote {
                base: 8,
                endpoints: [Some("2001:db8::1".parse().unwrap()), None],
                from_quoted_destination: false,
            },
            ports: (48, [NonZeroU16::new(12345), None]),
            checksums: [Some((2, 0xabcd)), Some((54, 0x5678)), None],
            masquerade_interface: None,
            fragmented_ipv6: true,
        };
        // 覆盖字段全部在片内、校验和横跨片边界，以及当前片完全没有改写。
        for (offset, length, expected_prefix) in [(0, 80, 56), (31, 24, 24), (55, 8, 1), (56, 8, 0)] {
            let mut expected = [0; 80];
            let mut split = expected;
            edit.payload(&mut expected[..length], offset);
            let prefix = edit.payload_prefix_len(offset, length);
            assert_eq!(prefix, expected_prefix);
            edit.payload(&mut split[..prefix], offset);
            assert_eq!(split, expected);
        }
    }
}
