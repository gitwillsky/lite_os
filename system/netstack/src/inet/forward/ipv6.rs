//! RFC 8200/4443 单播转发；端点扩展头不解读，路由器不分片，邻居与有界等待只使用原 owner。
use super::super::route::Ipv6InterfaceFacts;
use super::*;
use core::net::Ipv6Addr;
use packet_wire::{
    ipv6::options::UnknownOptionAction,
    ipv6::{Ipv6Datagram, Ipv6Header},
};

impl Forwarding {
    pub(in crate::inet) fn ingress_ipv6(
        &mut self,
        ports: &mut Ports<'_>,
        routing: &super::super::route::Ipv6Routes,
        (ip, checksum_valid): (Ipv6Datagram<'_>, bool),
        firewall: &mut Firewall,
        now: Instant,
        budget: &mut usize,
    ) {
        if !unicast(ip.destination())
            || !valid_source(ip.source(), ports.current.index, ports.ipv6_facts())
        {
            ports.device.record_receive_drop();
            return;
        }
        let Ok(flow) = self.admit(ports, firewall, (ip.destination().into(), ip.bytes().len()), || Packet::ipv6(ip, checksum_valid), now) else {
            ports.device.record_receive_drop();
            return;
        };
        let result = match hop_options(ip) {
            Ok(()) => send(
                ports,
                routing,
                ip,
                ip.hop_limit().saturating_sub(1),
                ports.current.index,
                now,
                budget,
            ),
            Err(result) => result,
        };
        let retained = match result {
            SendResult::Done => {
                if let Some(flow) = flow {
                    self.flows.confirm(flow, now);
                }
                true
            }
            SendResult::Pending(_) => self.enqueue(
                ip.bytes(),
                (ports.current.index, ports.mark, flow.map(|flow| flow.observation)),
                ip.hop_limit().saturating_sub(1),
                0,
                now,
                now + RETRY,
            ),
            SendResult::Yield(_) => self.enqueue(
                ip.bytes(),
                (ports.current.index, ports.mark, flow.map(|flow| flow.observation)),
                ip.hop_limit().saturating_sub(1),
                0,
                now,
                now,
            ),
            SendResult::Error(kind, code, parameter) => {
                self.error_ipv6(ports, routing, ip, (kind, code, parameter), now);
                false
            }
            SendResult::Discard => false,
        };
        if !retained {
            ports.device.record_receive_drop();
        }
    }
    pub(super) fn error_ipv6(
        &mut self,
        ports: &mut Ports<'_>,
        routing: &super::super::route::Ipv6Routes,
        ip: Ipv6Datagram<'_>,
        reason: (u8, u8, u32),
        now: Instant,
    ) {
        if !unicast(ip.source()) || ip.destination().is_multicast() {
            return;
        }
        let Ok(Some((protocol, payload))) = ip.upper_prefix() else {
            return;
        };
        if protocol == 58
            && payload
                .first()
                .is_none_or(|kind| *kind < 128 || *kind == 137)
        {
            return;
        }
        let bound = ip
            .source()
            .is_unicast_link_local()
            .then_some(ports.current.index);
        let Ok(route) = routing.select_output(
            Ipv6Addr::UNSPECIFIED,
            ip.source(),
            bound,
            now.total_millis().max(0) as u64,
            (0, ports.rules.local()),
            ports.ipv6_facts(),
        ) else {
            return;
        };
        let Some(source) = route.preferred_source.or_else(|| {
            ports
                .ipv6_facts()
                .find(|interface| interface.index == route.interface)
                .and_then(|interface| interface.addresses?.source_for(ip.source()))
        }) else {
            return;
        };
        if !self.spend_error(now) {
            return;
        }
        let mut bytes = [0; 1280];
        let quote = &ip.bytes()[..ip.bytes().len().min(1232)];
        let Ok(length) = packet_wire::icmpv6::emit_error(
            &mut bytes[40..],
            reason.0,
            reason.1,
            reason.2,
            (source, ip.source()),
            quote,
            &[],
        ) else {
            return;
        };
        let bytes = &mut bytes[..40 + length];
        if (Ipv6Header {
            source,
            destination: ip.source(),
            next_header: 58,
            hop_limit: 64,
            traffic_class: 0,
            flow_label: 0,
        })
        .emit(bytes)
        .is_err()
        {
            return;
        }
        ports
            .replies
            .enqueue_copy(bytes, ports.current.index, bound, now);
    }
}
fn unicast(address: Ipv6Addr) -> bool {
    !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_loopback()
        && address.to_ipv4_mapped().is_none()
}

fn valid_source<'a>(
    source: Ipv6Addr,
    ingress: u32,
    mut interfaces: impl Iterator<Item = Ipv6InterfaceFacts<'a>>,
) -> bool {
    unicast(source)
        && !interfaces.any(|interface| {
            (!source.is_unicast_link_local() || interface.index == ingress)
                && interface
                    .addresses
                    .is_some_and(|owner| owner.has_address(source))
        })
}

fn within_scope(source: Ipv6Addr, destination: Ipv6Addr, ingress: u32, egress: u32) -> bool {
    ingress == egress || !(source.is_unicast_link_local() || destination.is_unicast_link_local())
}

pub(super) fn input_path(
    network: &NetworkStack,
    ingress: &NetworkInterface,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    mark: u32,
    now: Instant,
) -> Result<RoutePath, SocketError> {
    if ingress.device.ip_mtu() < 1280 || ingress.ipv6.is_none() {
        return Err(SocketError::NetworkUnreachable);
    }
    let facts = network.interfaces.iter().map(Ipv6InterfaceFacts::capture);
    let loopback = matches!(ingress.device, ProtocolDevice::Loopback(_));
    if loopback {
        if !source.is_unspecified() && !network.has_local_ip(source.into(), None) {
            return Err(SocketError::Invalid);
        }
    } else if !unicast(destination)
        || !source.is_unspecified() && !valid_source(source, ingress.index, facts.clone())
    {
        return Err(SocketError::Invalid);
    }
    let route = network.ipv6_routing.select_transit(
        source,
        destination,
        (ingress.index, mark, network.selection_rules.transit()),
        now.total_millis().max(0) as u64,
        facts,
    );
    if (loopback || !network.forwarding_enabled.ipv6) && !matches!(route, Ok(path) if path.local) {
        return Err(SocketError::HostUnreachable);
    }
    let route = route?;
    if !route.local && !within_scope(source, destination, ingress.index, route.interface) {
        return Err(SocketError::HostUnreachable);
    }
    Ok(RoutePath {
        table: route.table,
        interface_index: if route.local { 1 } else { route.interface },
        kind: if route.local {
            RouteKind::Local
        } else {
            RouteKind::Unicast
        },
        source: source.into(),
        gateway: route.via_gateway.then_some(route.neighbor.into()),
    })
}

fn hop_options(ip: Ipv6Datagram<'_>) -> Result<(), SendResult> {
    let hop = ip
        .hop_by_hop()
        .map_err(|error| SendResult::Error(4, 0, error.offset as u32))?;
    if let Some(header) = hop {
        for option in header.options().expect("Hop-by-Hop options were validated") {
            match option.kind {
                0 | 1 | 0xc2 => {}
                5 if option.data.len() == 2 && option.offset.is_multiple_of(2) => {}
                5 => return Err(SendResult::Error(4, 0, option.offset as u32)),
                _ => match option.unknown_action() {
                    UnknownOptionAction::Skip => {}
                    UnknownOptionAction::Discard => return Err(SendResult::Discard),
                    UnknownOptionAction::DiscardAndIcmp
                    | UnknownOptionAction::DiscardAndIcmpUnlessMulticast => {
                        return Err(SendResult::Error(4, 2, option.offset as u32));
                    }
                },
            }
        }
    }
    Ok(())
}
pub(super) fn send(
    ports: &mut Ports<'_>,
    routing: &super::super::route::Ipv6Routes,
    ip: Ipv6Datagram<'_>,
    hop: u8,
    scope: u32,
    now: Instant,
    budget: &mut usize,
) -> SendResult {
    let route = match routing.select_transit(
        ip.source(),
        ip.destination(),
        (scope, ports.mark, ports.rules.transit()),
        now.total_millis().max(0) as u64,
        ports.ipv6_facts(),
    ) {
        Ok(route) => route,
        Err(SocketError::Invalid) => return SendResult::Discard,
        Err(SocketError::PermissionDenied) => return SendResult::Error(1, 1, 0),
        Err(SocketError::HostUnreachable) => return SendResult::Error(1, 3, 0),
        Err(_) => return SendResult::Error(1, 0, 0),
    };
    if route.local {
        return SendResult::Discard;
    }
    if hop == 0 {
        return SendResult::Error(3, 0, 0);
    }
    if !within_scope(ip.source(), ip.destination(), scope, route.interface) {
        return SendResult::Error(1, 2, 0);
    }
    let (owner, device) = if route.interface == ports.current.index {
        (&mut *ports.ipv6, ports.device)
    } else {
        let Some(output) = ports
            .before
            .iter_mut()
            .chain(ports.after.iter_mut())
            .find(|interface| interface.index == route.interface && interface.state.up)
        else {
            return SendResult::Error(1, 0, 0);
        };
        (&mut output.ipv6, &output.device)
    };
    let device_mtu = device.ip_mtu();
    let mtu = owner
        .as_ref()
        .map_or(device_mtu, |owner| owner.link_mtu(device_mtu));
    if ip.bytes().len() > mtu {
        return SendResult::Error(2, 0, mtu.min(u32::MAX as usize) as u32);
    }
    if *budget == 0 {
        return SendResult::Yield(0);
    }
    let destination = if matches!(device, ProtocolDevice::Ethernet(_)) {
        let Some(owner) = owner.as_mut() else {
            return SendResult::Error(1, 0, 0);
        };
        match owner.resolve(route.neighbor, now.total_millis().max(0) as u64) {
            Ok(Some(mac)) => Some(mac),
            Ok(None) => {
                return if owner.neighbor_work_due(now.total_millis().max(0) as u64) {
                    SendResult::Yield(0)
                } else {
                    SendResult::Pending(0)
                };
            }
            Err(SocketError::NoMemory) => return SendResult::Discard,
            Err(_) => return SendResult::Error(1, 3, 0),
        }
    } else {
        None
    };
    let Some(mut token) = device.transmit_shared(now) else {
        return SendResult::Pending(0);
    };
    *budget -= 1;
    use smoltcp::phy::TxToken;
    let mut metadata = smoltcp::phy::PacketMeta::default();
    metadata.interface_index = Some(route.interface);
    // 转发不修复或卸载上游 transport checksum；既有 payload（含认证/分片）只复制一次。
    metadata.tx_checksum = false;
    token.set_meta(metadata);
    let link_length = usize::from(destination.is_some()) * 14;
    let (_, failure) = token.consume_committed(link_length + ip.bytes().len(), 0x86dd, |output| {
        if let Some(destination) = destination {
            packet_wire::ethernet::EthernetHeader {
                source: device.mac_address(),
                destination,
                type_or_length: 0x86dd,
                tags: &[],
            }
            .emit(output)
            .expect("exact Ethernet IPv6 reservation");
        }
        output[link_length..].copy_from_slice(ip.bytes());
        output[link_length + 7] = hop;
    });
    match failure {
        None => {
            if let Some((index, gateway)) = route.recovery {
                if index == ports.current.index {
                    if let Some(owner) = ports.ipv6.as_mut() {
                        owner.probe_router(gateway, now.total_millis().max(0) as u64);
                    }
                } else {
                    let interfaces = if index < ports.current.index {
                        &mut *ports.before
                    } else {
                        &mut *ports.after
                    };
                    super::super::route::probe_gateway(interfaces, route.recovery, now);
                }
            }
            SendResult::Done
        }
        Some(device_model::network::NetworkError::WouldBlock) => SendResult::Pending(0),
        Some(_) => SendResult::Discard,
    }
}
