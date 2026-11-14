//! RFC 8200/4443 单播转发；端点扩展头不解读，路由器不分片，邻居与有界等待只使用原 owner。
use super::super::route::Ipv6InterfaceFacts;
use super::*;
use crate::inet::device::ReceivedPayload;
use core::net::Ipv6Addr;
use packet_wire::{
    ipv6::options::UnknownOptionAction,
    ipv6::{Ipv6Datagram, Ipv6Header},
};

impl Forwarding {
    pub(in crate::inet) fn ingress_ipv6(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        (ip, checksum): (Ipv6Datagram<'_>, ReceiveChecksum),
        firewall: &mut Firewall,
        now: Instant,
        budget: &mut usize,
    ) {
        let routing = routes.1;
        if !unicast(ip.destination())
            || !valid_source(ip.source(), ports.current.index, ports.ipv6_facts())
        {
            ports.device.record_receive_drop();
            return;
        }
        let prepared = ReceiveHeader::from_frame(ports.receive_frame);
        let mut hop = prepared.ipv6_hop();
        if self.inspection(firewall, true).required()
            && prepared.ipv6_fragment().unwrap_or_else(|| ip.has_fragment_header())
        {
            if let Err(result) = hop.take().unwrap_or_else(|| hop_options(ip)) {
                if let SendResult::Error(kind, code, parameter) = result {
                    self.error_ipv6(ports, routing, ip, (kind, code, parameter), now);
                }
                ports.device.record_receive_drop();
                return;
            }
            hop = Some(Ok(()));
            if self.accept_fragments(ports, routes, firewall, ip.bytes(), now, budget) {
                return;
            }
        }
        let Ok(Some(Admission {
            flow,
            rewrite,
            path,
        })) = self.admit(
            ports,
            (routes, firewall),
            (ip.source().into(), ip.destination().into(), ip.bytes()),
            (None, false),
            |receive| Packet::ipv6(ip, checksum, receive),
            now,
        )
        else {
            ports.device.record_receive_drop();
            return;
        };
        let result = match hop.unwrap_or_else(|| hop_options(ip)) {
            Ok(()) => TransitPacket::Ipv6(ip).forward(ports, routes, (rewrite, path), now, budget),
            Err(result) => result,
        };
        self.finish_ingress(
            ports,
            routes,
            PacketData::Single(ip.bytes()),
            (result, flow, rewrite),
            now,
        );
    }
    pub(super) fn error_ipv6(
        &mut self,
        ports: &mut Ports<'_>,
        routing: &super::super::route::Ipv6Routes,
        ip: Ipv6Datagram<'_>,
        reason: (u8, u8, u32),
        now: Instant,
    ) {
        if !unicast(ip.source())
            || ip.destination().is_multicast() && !matches!((reason.0, reason.1), (2, 0) | (4, 2))
        {
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
                .get(route.interface)
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
pub(super) fn unicast(address: Ipv6Addr) -> bool {
    !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_loopback()
        && address.to_ipv4_mapped().is_none()
}

pub(super) fn valid_source<'a>(
    source: Ipv6Addr,
    ingress: u32,
    interfaces: Ipv6Interfaces<'a>,
) -> bool {
    let owns_source = |interface: Ipv6InterfaceFacts<'_>| {
        interface
            .addresses
            .is_some_and(|owner| owner.has_address(source))
    };
    unicast(source)
        && if source.is_unicast_link_local() {
            !interfaces.get(ingress).is_some_and(owns_source)
        } else {
            !interfaces.iter().any(owns_source)
        }
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
    let facts = Ipv6Interfaces::live(&network.interfaces);
    let loopback = matches!(ingress.device, ProtocolDevice::Loopback(_));
    if loopback {
        if !source.is_unspecified() && !network.has_local_ip(source.into(), None) {
            return Err(SocketError::Invalid);
        }
    } else if !unicast(destination)
        || !source.is_unspecified() && !valid_source(source, ingress.index, facts)
    {
        return Err(SocketError::Invalid);
    }
    let route = network.ipv6_routing.select_transit(
        source,
        (destination, 0),
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

pub(super) fn hop_options(ip: Ipv6Datagram<'_>) -> Result<(), SendResult> {
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
                    UnknownOptionAction::DiscardAndIcmpUnlessMulticast
                        if ip.destination().is_multicast() =>
                    {
                        return Err(SendResult::Discard);
                    }
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
pub(super) fn select<'a>(
    routing: &super::super::route::Ipv6Routes,
    (source, destination, probe): (Ipv6Addr, Ipv6Addr, u64),
    policy: TransitPolicy<'_>,
    interfaces: Ipv6Interfaces<'a>,
    now: Instant,
) -> Result<super::super::route::Ipv6Selection, SendResult> {
    let route = routing.select_transit(
        source,
        (destination, probe),
        policy,
        now.total_millis().max(0) as u64,
        interfaces,
    );
    match route {
        Ok(route) if !route.local => Ok(route),
        Ok(_) | Err(SocketError::Invalid) => Err(SendResult::Discard),
        Err(SocketError::PermissionDenied) => Err(SendResult::Error(1, 1, 0)),
        Err(SocketError::HostUnreachable) => Err(SendResult::Error(1, 3, 0)),
        Err(_) => Err(SendResult::Error(1, 0, 0)),
    }
}

pub(super) fn send(
    ports: &mut Ports<'_>,
    route: super::super::route::Ipv6Selection,
    ip: Ipv6Datagram<'_>,
    rewrite: Option<Rewrite>,
    now: Instant,
    budget: &mut usize,
) -> SendResult {
    let scope = ports.current.index;
    let receive = (ports.device, ports.ingress_queue);
    let receive_frame = ports.receive_frame;
    let (source, destination) =
        rewrite.map_or_else(|| (ip.source(), ip.destination()), |edit| edit.ipv6(ip));
    let hop = ip.hop_limit().saturating_sub(1);
    if hop == 0 {
        return SendResult::Error(3, 0, 0);
    }
    if !within_scope(source, destination, scope, route.interface) {
        return SendResult::Error(1, 2, 0);
    }
    let (owner, device) = if route.interface == ports.current.index {
        (&mut *ports.ipv6, ports.device)
    } else {
        let Some(output) = ports.other_output(route.interface) else {
            return SendResult::Error(1, 0, 0);
        };
        (&mut output.ipv6, &output.device)
    };
    if rewrite
        .and_then(|edit| edit.masquerade_interface)
        .is_some_and(|index| {
            index.get() != route.interface
                || !owner
                    .as_ref()
                    .is_some_and(|owner| owner.has_address(source))
        })
    {
        return SendResult::Discard;
    }
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
    let target = if matches!(device, ProtocolDevice::Ethernet(_)) {
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
    let patch = if let Some(edit) = rewrite {
        let Some((prefix, offset)) = edit.ipv6_payload(ip) else {
            return SendResult::Discard;
        };
        Some((edit, prefix, offset))
    } else {
        None
    };
    let token = match device.reserve(|count| {
        TransitPacket::Ipv6(ip).transmit_queue(receive, count)
    }) {
        Ok(token) => token,
        Err(device_model::network::NetworkError::WouldBlock) => return SendResult::Pending(0),
        Err(_) => return SendResult::Discard,
    };
    *budget -= 1;
    let (fixed, bytes) = ip.bytes().split_at(40);
    let mut header = <[u8; 40]>::try_from(fixed).expect("validated IPv6 fixed header");
    header[7] = hop;
    if patch.is_some() {
        header[8..24].copy_from_slice(&source.octets());
        header[24..40].copy_from_slice(&destination.octets());
    }
    let original = receive_frame.map(|frame| {
        let body_prefix = patch.map_or(0, |(edit, prefix, offset)| {
            let extensions = prefix - 40;
            let modified = edit.payload_prefix_len(offset, bytes.len() - extensions);
            if modified == 0 {
                0
            } else {
                extensions + modified
            }
        });
        ReceivedPayload {
            frame,
            bytes,
            render_prefix: body_prefix,
        }
    });
    let result = token.submit_ip(
        target.map(|destination| (device.mac_address(), destination)),
        &header,
        (
            bytes.len(),
            |output: &mut [u8]| {
                output.copy_from_slice(&bytes[..output.len()]);
                if let Some((edit, prefix, offset)) = patch
                    && let Some(output) = output.get_mut(prefix - 40..)
                {
                    edit.payload(output, offset);
                }
            },
            original,
        ),
        (route.interface, false),
    );
    match result {
        Ok(()) => {
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
        Err(device_model::network::NetworkError::WouldBlock) => SendResult::Pending(0),
        Err(_) => SendResult::Discard,
    }
}
