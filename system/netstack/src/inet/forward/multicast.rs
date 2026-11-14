//! 静态组播转发。精确源与任意源共享一张表，入接口是反向路径检查的权威。
use super::*;
use packet_wire::ethernet::EthernetFrame;
use router_protocol::multicast::{Key, Route, valid_key};

/// 由 NET 配置租约拥有的完整组播表。
#[derive(Default)]
pub(in crate::inet) struct Table(Vec<Route>);

impl Table {
    /// 捕获候选表。非法正文或内存不足时保持原表。
    pub(in crate::inet) fn prepare(bytes: &[u8]) -> Result<Self, SocketError> {
        router_protocol::multicast::decode(bytes)
            .map(Self)
            .map_err(|error| match error {
                router_protocol::routing::EncodeError::Invalid => SocketError::Invalid,
                router_protocol::routing::EncodeError::NoMemory => SocketError::NoMemory,
            })
    }

    /// 校验候选域的有效三层接口。零入口表示配置引用的设备尚未出现。
    pub(in crate::inet) fn validate_interfaces(
        &self,
        interfaces: &[NetworkInterface],
        switching: &crate::inet::switching::Switching,
    ) -> Result<(), SocketError> {
        for route in &self.0 {
            for index in core::iter::once(route.ingress)
                .chain(route.outputs.iter().copied())
                .filter(|&index| index != 0)
            {
                let position = interfaces
                    .binary_search_by_key(&index, |interface| interface.index)
                    .map_err(|_| SocketError::NoDevice)?;
                let interface = &interfaces[position];
                if interface.device.vlan().is_some() && !switching.has_gateway(index)
                    || switching.contains(index)
                    || !matches!(interface.device, ProtocolDevice::Ethernet(_))
                {
                    return Err(SocketError::Invalid);
                }
            }
        }
        Ok(())
    }

    pub(in crate::inet) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn outputs(&self, group: IpAddr, source: IpAddr, ingress: u32) -> &[u32] {
        let lookup = |key: Key| self.0.binary_search_by_key(&key, |route| route.key);
        lookup((group, Some(source)))
            .or_else(|_| lookup((group, None)))
            .ok()
            .map(|position| &self.0[position])
            .filter(|route| route.ingress == ingress)
            .map_or(&[], |route| route.outputs.as_slice())
    }

    /// 从原始链路包复制转发副本。原包仍进入同一接口的本机订阅和控制协议处理。
    pub(in crate::inet) fn ingress(
        &self,
        ports: &mut Ports<'_>,
        (forwarding, firewall): (&mut Forwarding, &mut Firewall),
        routes: Routes<'_>,
        (frame, checksum): (&[u8], ReceiveChecksum),
        enabled: network_protocol::Forwarding,
        (now, budget): (Instant, &mut usize),
    ) {
        let Ok(frame) = EthernetFrame::parse(frame) else {
            return;
        };
        let source_mac = frame.source();
        if source_mac == [0; 6] || source_mac[0] & 1 != 0 {
            return;
        }
        let index = ports.current.index;
        match frame.type_or_length() {
            0x0800 if enabled.ipv4 => {
                let Ok(ip) = Ipv4Packet::parse(frame.payload()) else {
                    return;
                };
                let (source, group) = (ip.source(), ip.destination());
                let octets = group.octets();
                if ip.hop_limit() <= 1
                    || frame.destination() != [1, 0, 0x5e, octets[1] & 0x7f, octets[2], octets[3]]
                    || !valid_key((group.into(), Some(source.into())))
                {
                    return;
                }
                let outputs = self.outputs(group.into(), source.into(), index);
                if outputs.is_empty()
                    || !ipv4::valid_source(routes.0, (source, ports.source_prefix(source)), ports.facts())
                    || !allows(
                        firewall,
                        index,
                        (source.into(), group.into()),
                        ip.bytes().len(),
                        ip.fragmented(),
                        || Packet::ipv4(ip, checksum, None),
                    )
                {
                    return;
                }
                for output in outputs {
                    if *budget == 0 {
                        break;
                    }
                    if let Some(interface) = output_interface(ports, *output) {
                        send_ipv4(interface, ip, now, budget);
                    }
                }
            }
            0x86dd if enabled.ipv6 => {
                let Ok(ip) = Ipv6Datagram::parse(frame.payload()) else {
                    return;
                };
                let (source, group) = (ip.source(), ip.destination());
                let octets = group.octets();
                if ip.hop_limit() <= 1
                    || frame.destination()
                        != [0x33, 0x33, octets[12], octets[13], octets[14], octets[15]]
                    || !valid_key((group.into(), Some(source.into())))
                {
                    return;
                }
                let outputs = self.outputs(group.into(), source.into(), index);
                if outputs.is_empty()
                    || !ipv6::valid_source(source, index, ports.ipv6_facts())
                    || !allows(
                        firewall,
                        index,
                        (source.into(), group.into()),
                        ip.bytes().len(),
                        ip.has_fragment_header(),
                        || Packet::ipv6(ip, checksum, None),
                    )
                {
                    return;
                }
                if let Err(result) = ipv6::hop_options(ip) {
                    if let SendResult::Error(4, 2, pointer) = result {
                        forwarding.error_ipv6(ports, routes.1, ip, (4, 2, pointer), now);
                    }
                    return;
                }
                let mut smallest = None;
                for output in outputs {
                    if *budget == 0 {
                        break;
                    }
                    if let Some(interface) = output_interface(ports, *output)
                        && let Some(mtu) = send_ipv6(interface, ip, now, budget)
                    {
                        smallest = Some(smallest.map_or(mtu, |old: u32| old.min(mtu)));
                    }
                }
                if let Some(mtu) = smallest {
                    forwarding.error_ipv6(ports, routes.1, ip, (2, 0, mtu), now);
                }
            }
            _ => {}
        }
    }
}

fn output_interface<'a>(ports: &'a mut Ports<'_>, index: u32) -> Option<&'a mut NetworkInterface> {
    let side = if ports
        .before
        .last()
        .is_some_and(|entry| index <= entry.index)
    {
        &mut *ports.before
    } else {
        &mut *ports.after
    };
    let position = side
        .binary_search_by_key(&index, |entry| entry.index)
        .ok()?;
    let interface = &mut side[position];
    interface.link_active().then_some(interface)
}

fn allows(
    firewall: &mut Firewall,
    ingress: u32,
    addresses: (IpAddr, IpAddr),
    length: usize,
    fragmented: bool,
    transport: impl FnOnce() -> Option<Packet>,
) -> bool {
    let inspection = firewall.inspection(addresses.1.is_ipv6());
    if inspection.transport && fragmented {
        return false;
    }
    let transport = inspection
        .transport
        .then(transport)
        .flatten()
        .as_ref()
        .and_then(Packet::transport);
    // 组播有多个接收端，无单一反方向流或可转换的单播身份。
    firewall
        .evaluate(((ingress, addresses, (None, transport)), length), None)
        .is_some_and(|(_, translation)| translation.is_none())
}

fn send_ipv4(
    interface: &mut NetworkInterface,
    ip: Ipv4Packet<'_>,
    now: Instant,
    budget: &mut usize,
) {
    let header_length = ip.bytes().len() - ip.payload().len();
    let mut header = [0; 60];
    header[..header_length].copy_from_slice(&ip.bytes()[..header_length]);
    header[8] -= 1;
    if ipv4::options(&mut header[..header_length], interface.state.address, now).is_err() {
        return;
    }
    let mut offset = 0;
    while *budget != 0 {
        let mut fragment = [0; 60];
        let Ok(range) = packet_wire::ipv4::fragment::header(
            &header[..header_length],
            ip.payload().len(),
            offset,
            interface.device.ip_mtu(),
            &mut fragment,
        ) else {
            break;
        };
        *budget -= 1;
        let body = &ip.payload()[range.clone()];
        if interface
            .device
            .transmit_ipv4(
                (&mut interface.arp, interface.state, &mut interface.ipv6),
                &fragment[..header_length],
                (body.len(), |output: &mut [u8]| output.copy_from_slice(body), None),
                (ip.destination().into(), interface.index),
                now,
                (false, |_| 0),
            )
            .is_err()
        {
            break;
        }
        offset = range.end;
        if offset == ip.payload().len() {
            break;
        }
    }
}

fn send_ipv6(
    interface: &mut NetworkInterface,
    ip: Ipv6Datagram<'_>,
    now: Instant,
    budget: &mut usize,
) -> Option<u32> {
    let owner = interface.ipv6.as_ref()?;
    let mtu = owner.link_mtu(interface.device.ip_mtu());
    if mtu < 1280 {
        return None;
    }
    if ip.bytes().len() > mtu {
        return Some(mtu as u32);
    }
    *budget -= 1;
    let (fixed, body) = ip.bytes().split_at(40);
    let mut header: [u8; 40] = fixed.try_into().expect("validated IPv6 header");
    header[7] -= 1;
    let _ = interface.device.transmit_ipv6(
        &mut interface.ipv6,
        &header,
        (body.len(), |output| output.copy_from_slice(body)),
        (ip.destination(), interface.index),
        now,
        false,
    );
    None
}
