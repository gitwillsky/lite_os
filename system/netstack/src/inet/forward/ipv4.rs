//! IPv4 单播执行、选项/分片及错误回复；队列、预算与限速仍属于父级唯一 owner。
use super::*;
use crate::inet::device::ReceivedPayload;
use packet_wire::ipv4::Ipv4Header;

impl Forwarding {
    /// 转发已验证且目的不是本机的原包；正常成功不分配，临时失败只保存尚未发出的分片游标。
    /// `ports`/`routing` 必须属于同域同一 NET guard，`ip` 已校验 checksum；`now` 单调，`budget` 属于本轮全部转发。
    /// 不返回协议状态副本；无法保留或被拒绝的原包计入口 RX drop，ICMP 仅 best-effort。
    pub(in crate::inet) fn ingress_ipv4(
        &mut self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        (ip, checksum): (Ipv4Packet<'_>, ReceiveChecksum),
        firewall: &mut Firewall,
        now: Instant,
        budget: &mut usize,
    ) {
        let routing = routes.0;
        let source = ip.source();
        if !unicast(ip.destination())
            || !valid_source(routing, (source, ports.source_prefix(source)), ports.facts())
        {
            ports.device.record_receive_drop();
            return;
        }
        if ip.fragmented()
            && self.inspection(firewall, false).required()
            && self.accept_fragments(ports, routes, firewall, ip.bytes(), now, budget)
        {
            return;
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
            |receive| Packet::ipv4(ip, checksum, receive),
            now,
        )
        else {
            ports.device.record_receive_drop();
            return;
        };
        let result = TransitPacket::Ipv4(ip).forward(ports, routes, (rewrite, path), now, budget);
        self.finish_ingress(
            ports,
            routes,
            PacketData::Single(ip.bytes()),
            (result, flow, rewrite),
            now,
        );
    }
    pub(super) fn error_ipv4(
        &mut self,
        ports: &mut Ports<'_>,
        routing: &RoutingTable,
        ip: Ipv4Packet<'_>,
        reason: (u8, u8, u32),
        now: Instant,
    ) {
        let (kind, code, parameter) = reason;
        // 1. 不反射非法源、后续片或 ICMP 错误；本入口只接本机单播 MAC，链路广播未进入转发。
        if !unicast(ip.source())
            || ip.fragment_offset() != 0
            || ip.protocol() == 1
                && ip
                    .payload()
                    .first()
                    .is_some_and(|kind| matches!(kind, 3 | 4 | 5 | 11 | 12))
        {
            return;
        }
        let Ok(route) = routing.select_output(
            Ipv4Addr::UNSPECIFIED,
            ip.source(),
            None,
            (0, ports.rules.local()),
            ports.facts(),
        ) else {
            return;
        };
        let Some(source) = route.preferred_source.or_else(|| {
            ports
                .facts()
                .get(route.interface_index)
                .and_then(|facts| facts.address)
        }) else {
            return;
        };
        if !self.spend_error(now) {
            return;
        }
        // 2. 最多 576 字节；引用仍是减 TTL/修改选项前的原包。小 MTU 下错误本身沿同一分片发送。
        let mut bytes = [0; 576];
        let quote = &ip.bytes()[..ip.bytes().len().min(548)];
        let Ok(length) =
            packet_wire::icmpv4::emit_error(&mut bytes[20..], kind, code, parameter, quote, &[])
        else {
            return;
        };
        let bytes = &mut bytes[..20 + length];
        let header = Ipv4Header {
            source,
            destination: ip.source(),
            protocol: 1,
            hop_limit: 64,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0,
            options: &[],
        };
        if header.emit(bytes).is_err() {
            return;
        }
        // 3. 本机错误统一交 IP 回复 owner；只有转发的原包保留对端标识及转发等待队列。
        ports
            .replies
            .enqueue_copy(bytes, ports.current.index, None, now);
    }
}
pub(super) fn unicast(address: Ipv4Addr) -> bool {
    let bytes = address.octets();
    bytes[0] != 0 && bytes[0] != 127 && bytes[0] < 224
}

pub(super) fn valid_source(
    routing: &RoutingTable,
    (source, probe): (Ipv4Addr, u64),
    interfaces: Ipv4Interfaces<'_>,
) -> bool {
    unicast(source)
        && !matches!(
            routing.destination_kind(Ipv4Addr::UNSPECIFIED, (source, probe), interfaces),
            Some(RouteKind::Broadcast | RouteKind::Local)
        )
}

pub(super) fn input_path(
    network: &NetworkStack,
    ingress: &NetworkInterface,
    source: Ipv4Addr,
    destination: Ipv4Addr,
    mark: u32,
) -> Result<RoutePath, SocketError> {
    if destination.is_broadcast() {
        return Err(SocketError::OperationNotSupported);
    }
    let facts = Ipv4Interfaces::live(&network.interfaces);
    let loopback = matches!(ingress.device, ProtocolDevice::Loopback(_));
    if loopback {
        if !network.has_local_address(source) {
            return Err(SocketError::Invalid);
        }
    } else if !unicast(destination) || !valid_source(&network.routing, (source, 0), facts) {
        return Err(SocketError::Invalid);
    }
    let route = network.routing.select_transit(
        source,
        (destination, 0, None),
        (ingress.index, mark, network.selection_rules.transit()),
        facts,
    );
    if (loopback || !network.forwarding_enabled.ipv4)
        && !matches!(route, Ok(path) if matches!(path.kind, RouteKind::Local | RouteKind::Broadcast))
    {
        return Err(SocketError::HostUnreachable);
    }
    let route = route.map_err(super::super::route::selection_error)?;
    Ok(RoutePath {
        table: route.table,
        interface_index: route.interface_index,
        kind: route.kind,
        source: source.into(),
        gateway: route.via_gateway.then_some(route.next_hop),
    })
}

pub(super) fn select(
    routing: &RoutingTable,
    source: Ipv4Addr,
    destination: (Ipv4Addr, u64, Option<&super::super::route::ReceiveDestination>),
    policy: TransitPolicy<'_>,
    interfaces: Ipv4Interfaces<'_>,
) -> Result<super::super::route::RouteSelection, SendResult> {
    let route = routing.select_transit(source, destination, policy, interfaces);
    match route {
        Ok(route) => Ok(route),
        Err(RouteKind::Blackhole) => Err(SendResult::Discard),
        Err(RouteKind::Prohibit) => Err(SendResult::Error(3, 13, 0)),
        Err(_) => Err(SendResult::Error(3, 0, 0)),
    }
}

pub(super) fn send(
    ports: &mut Ports<'_>,
    route: super::super::route::RouteSelection,
    ip: Ipv4Packet<'_>,
    (mut offset, rewrite): (usize, Option<Rewrite>),
    now: Instant,
    budget: &mut usize,
) -> SendResult {
    let receive = (ports.device, ports.ingress_queue);
    let receive_frame = ports.receive_frame;
    let (source, destination) =
        rewrite.map_or_else(|| (ip.source(), ip.destination()), |edit| edit.ipv4(ip));
    let hop = ip.hop_limit().saturating_sub(1);
    if hop == 0 {
        return SendResult::Error(11, 0, 0);
    }
    if route.kind != RouteKind::Unicast {
        return SendResult::Discard;
    }
    let (arp, device, state, ipv6) = if route.interface_index == ports.current.index {
        (
            &mut *ports.ipv4.0,
            ports.device,
            ports.ipv4.1,
            &mut *ports.ipv6,
        )
    } else {
        let Some(output) = ports.other_output(route.interface_index) else {
            return SendResult::Error(3, 0, 0);
        };
        (
            &mut output.arp,
            &output.device,
            output.state,
            &mut output.ipv6,
        )
    };
    if rewrite
        .and_then(|edit| edit.masquerade_interface)
        .is_some_and(|index| index.get() != route.interface_index || state.address != Some(source))
    {
        return SendResult::Discard;
    }
    let mtu = device.ip_mtu();
    let header_length = ip.bytes().len() - ip.payload().len();
    if ip.bytes().len() > mtu && ip.bytes()[6] & 0x40 != 0 {
        return SendResult::Error(3, 4, mtu.min(65535) as u32);
    }
    if mtu < header_length + 8 {
        return SendResult::Error(3, 4, mtu.min(65535) as u32);
    }
    let direct_header = if offset == 0 && rewrite.is_none() && ip.bytes().len() <= mtu {
        ip.forwarded_base_header()
    } else {
        None
    };
    let mut header = [0; 60];
    if direct_header.is_none() {
        header[..header_length].copy_from_slice(&ip.bytes()[..header_length]);
        header[8] = hop;
        if rewrite.is_some() {
            header[12..16].copy_from_slice(&source.octets());
            header[16..20].copy_from_slice(&destination.octets());
        }
        if let Err(error) = options(&mut header[..header_length], state.address, now) {
            return match error {
                OptionsError::SourceRoute => SendResult::Error(3, 5, 0),
                OptionsError::Parameter(pointer) => {
                    SendResult::Error(12, 0, u32::from(pointer) << 24)
                }
            };
        }
    }
    loop {
        if *budget == 0 {
            return SendResult::Yield(offset);
        }
        let mut fragment = [0; 60];
        let (wire_header, length) = if let Some(header) = &direct_header {
            (&header[..], ip.payload().len())
        } else {
            let Ok(range) = packet_wire::ipv4::fragment::header(
                &header[..header_length],
                ip.payload().len(),
                offset,
                mtu,
                &mut fragment,
            ) else {
                return SendResult::Discard;
            };
            (&fragment[..header_length], range.len())
        };
        *budget -= 1;
        let bytes = &ip.payload()[offset..offset + length];
        let edit_offset = ip.fragment_offset() + offset;
        let original = receive_frame
            .filter(|_| offset == 0 && length == ip.payload().len())
            .map(|frame| ReceivedPayload {
                frame,
                bytes,
                render_prefix: rewrite.map_or(0, |edit| edit.payload_prefix_len(edit_offset, length)),
            });
        match device.transmit_ipv4(
            (arp, state, ipv6),
            wire_header,
            (
                length,
                |body: &mut [u8]| {
                    body.copy_from_slice(&bytes[..body.len()]);
                    if let Some(edit) = rewrite {
                        edit.payload(body, edit_offset);
                    }
                },
                original,
            ),
            (route.next_hop, route.logical_interface_index),
            now,
            (false, |count| {
                TransitPacket::Ipv4(ip).transmit_queue(receive, count)
            }),
        ) {
            Ok(()) => {}
            Err(device_model::network::NetworkError::WouldBlock) => {
                return SendResult::Pending(offset);
            }
            Err(_) => return SendResult::Discard,
        }
        offset += length;
        if offset == ip.payload().len() {
            return SendResult::Done;
        }
    }
}
pub(super) enum OptionsError {
    SourceRoute,
    Parameter(u8),
}
pub(super) fn options(
    header: &mut [u8],
    address: Option<Ipv4Addr>,
    now: Instant,
) -> core::result::Result<(), OptionsError> {
    let mut at = 20;
    while at < header.len() {
        let kind = header[at];
        if kind == 0 {
            break;
        }
        if kind == 1 {
            at += 1;
            continue;
        }
        let length = usize::from(header[at + 1]);
        // 源路由默认禁止；由调用者生成 Source Route Failed，而不是把它当普通目的继续转发。
        if kind == 131 || kind == 137 {
            return Err(OptionsError::SourceRoute);
        }
        if kind == 7 || kind == 68 {
            let base = if kind == 7 { 3 } else { 4 };
            if length < base {
                return Err(OptionsError::Parameter((at + 1) as u8));
            }
            let pointer = usize::from(header[at + 2]);
            if pointer <= base {
                return Err(OptionsError::Parameter((at + 2) as u8));
            }
            let flag = if kind == 68 { header[at + 3] & 15 } else { 0 };
            if kind == 68 && !matches!(flag, 0 | 1 | 3) {
                return Err(OptionsError::Parameter((at + 3) as u8));
            }
            let width = if kind == 7 || flag == 0 { 4 } else { 8 };
            if pointer <= length {
                if pointer - 1 + width > length {
                    return Err(OptionsError::Parameter((at + 2) as u8));
                }
                let slot = at + pointer - 1;
                let address = address.ok_or(OptionsError::Parameter(at as u8))?.octets();
                if kind == 7 {
                    header[slot..slot + 4].copy_from_slice(&address);
                } else {
                    if flag == 3 && header[slot..slot + 4] != address {
                        at += length;
                        continue;
                    }
                    if flag == 1 {
                        header[slot..slot + 4].copy_from_slice(&address);
                    }
                    let timestamp = ((now.total_millis() as u32) & 0x7fff_ffff) | 0x8000_0000;
                    let time = slot + width - 4;
                    header[time..time + 4].copy_from_slice(&timestamp.to_be_bytes());
                }
                header[at + 2] = (pointer + width) as u8;
            } else if kind == 68 {
                let overflow = header[at + 3] >> 4;
                if overflow == 15 {
                    return Err(OptionsError::Parameter((at + 3) as u8));
                }
                header[at + 3] += 16;
            }
        }
        at += length;
    }
    Ok(())
}
