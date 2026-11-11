//! TCP 的实际收发接入；连接直接属于 endpoint，路径与邻居只借本 NET 的唯一 owner。
use super::super::{interface::NetworkInterface, route::Output};
use super::*;
use alloc::sync::Arc;
use core::net::IpAddr;
use packet_wire::{ipv4::Ipv4Header, ipv6::Ipv6Header, tcp::TcpPacket};
use smoltcp::{
    phy::{Device, Medium, TxToken},
    time::Duration,
};
type Reply = (
    transport::IpPacket,
    transport::Segment<'static>,
    (Option<u32>, u32),
    Option<Arc<transport::Md5Key>>,
);
const RETRY: Duration = Duration::from_millis(1000);

impl TcpHandle {
    /// 半连接的策略由实际子连接持有，监听端后续修改仅影响新连接。
    fn egress(&self, endpoint_mark: u32) -> (Option<u32>, u32) {
        (
            self.transport.bound_interface(),
            self.passive
                .as_ref()
                .map_or(endpoint_mark, |policy| policy.options.mark),
        )
    }
}

/// 在 IP/RAW 已交付之后应用本机 TCP 输入；无效段不改变连接，无接收者时按 RAW/RST 约束决定回复。
/// 反向 MTU 只借本帧同一 FIB 已冻结的事实，不能再查询旧路径或猜测接收设备 MTU。
pub(in crate::inet) fn ingress(
    endpoints: &mut FallibleMap<usize, TcpEndpointState>,
    packet: smoltcp::iface::Ipv4Ingress<'_>,
    timestamp: Instant,
    key: &SequenceKey,
    marked_route: impl Fn(Option<u32>, u32) -> Option<smoltcp::iface::IpRouteContext>,
) -> Option<Reply> {
    let source = super::super::from_ip(packet.source);
    let destination = super::super::from_ip(packet.destination);
    if packet.multicast
        || source.is_unspecified()
        || source.is_multicast()
        || source.is_broadcast()
        || destination.is_unspecified()
    {
        return None;
    }
    let parsed = if packet.checksum_valid {
        TcpPacket::parse_header(packet.payload)
    } else {
        TcpPacket::parse(packet.payload, source.into(), destination.into())
    }
    .ok()?;
    let segment = transport::IncomingSegment::capture(parsed).ok()?;
    let ip = transport::IpPacket::new(
        source.into(),
        destination.into(),
        6,
        packet.payload.len(),
        Some(packet.hop_limit),
    );
    let metrics = |route: &smoltcp::iface::IpRouteContext| {
        (
            route.mtu,
            network_protocol::RouteMetrics {
                advertised_mss: route.advertised_mss as u32,
                receive_window: route.receive_window as u32,
                round_trip_time: route.initial_rtt,
            },
        )
    };
    receive_segment(
        endpoints,
        ip,
        segment,
        Path::new(
            timestamp,
            packet.interface_index,
            None,
            Some(destination.into()),
            key,
        ),
        |device, mark| {
            if mark != 0 {
                return marked_route(device, mark).as_ref().map(metrics);
            }
            match device {
                Some(_) => packet.bound_reverse_route,
                None => packet.reverse_route,
            }
            .map(metrics)
        },
        packet.handled_by_raw,
    )
}
/// IPv6 的本地 IP 准入完成后沿同一连接分派；反向路径只借本次冻结的 FIB 事实。
pub(in crate::inet) fn ingress_ipv6(
    endpoints: &mut FallibleMap<usize, TcpEndpointState>,
    packet: packet_wire::ipv6::Ipv6Packet<'_>,
    timestamp: Instant,
    logical: u32,
    reverse_route: impl Fn(Option<u32>, u32) -> Option<(usize, network_protocol::RouteMetrics)>,
    checksum_valid: bool,
    key: &SequenceKey,
) -> Option<Reply> {
    if packet.destination().is_multicast()
        || packet.source().is_unspecified()
        || packet.source().is_multicast()
    {
        return None;
    }
    let bytes = packet.upper_layer()?;
    let parsed = if checksum_valid {
        TcpPacket::parse_header(bytes)
    } else {
        TcpPacket::parse(bytes, packet.source().into(), packet.destination().into())
    }
    .ok()?;
    let segment = transport::IncomingSegment::capture(parsed).ok()?;
    let ip = transport::IpPacket::new(
        packet.source().into(),
        packet.destination().into(),
        6,
        bytes.len(),
        Some(packet.hop_limit()),
    );
    receive_segment(
        endpoints,
        ip,
        segment,
        Path::new(
            timestamp,
            logical,
            None,
            Some(packet.destination().into()),
            key,
        ),
        reverse_route,
        false,
    )
}
fn receive_segment<'a>(
    endpoints: &mut FallibleMap<usize, TcpEndpointState>,
    ip: transport::IpPacket,
    segment: transport::IncomingSegment<'_>,
    path: Path<'a>,
    reverse_route: impl Fn(Option<u32>, u32) -> Option<(usize, network_protocol::RouteMetrics)>,
    raw: bool,
) -> Option<Reply> {
    use transport::Context;
    let ipv6 = ip.src_addr().is_ipv6();
    let timestamp = Instant::from_micros(path.now().total_micros());
    let mut response = None;
    let matched = endpoints.try_for_each_mut::<()>(|_, state| {
        if (state.family == InetFamily::Ipv4 && ipv6)
            || (state.family == InetFamily::Ipv6 && state.options.ipv6_only && !ipv6)
        {
            return Ok(());
        }
        if let TcpMode::Listening { endpoint, .. } = state.mode
            && endpoint.ip().to_canonical().is_ipv4()
            && ipv6
        {
            return Ok(());
        }
        for entry in &mut state.handles {
            if !entry.transport.accepts_incoming(&path, &ip, &segment) {
                continue;
            }
            let scoped = matches!(ip.src_addr(),IpAddr::V6(ip) if ip.is_unicast_link_local())
                || matches!(ip.dst_addr(),IpAddr::V6(ip) if ip.is_unicast_link_local());
            let listening = entry.transport.state() == State::Listen;
            let device = entry
                .transport
                .bound_interface()
                .or_else(|| (listening && scoped).then_some(path.interface_index()));
            let mark = entry.egress(state.options.mark).1;
            let keys = entry
                .passive
                .as_ref()
                .map_or(&state.md5_keys, |policy| &policy.md5_keys);
            // 未验证的 SYN 不能给空闲 listener 槽留下其他邻居的关联。
            let mut opening = None;
            let binding = if listening {
                &mut opening
            } else {
                &mut entry.md5
            };
            Binding::refresh(binding, keys, ip.src_addr(), path.interface_index());
            if binding
                .as_ref()
                .is_some_and(|binding| !binding.permits(path.interface_index()))
            {
                return Err(());
            }
            let key = binding.as_ref().map(|binding| &binding.key);
            let Some(verified) =
                segment.authenticate(ip.src_addr(), ip.dst_addr(), key.map(Arc::as_ref))
            else {
                return Err(());
            };
            let mut context = path.with_route(reverse_route(device, mark));
            entry.transport.set_md5(key.is_some());
            if listening
                && segment.control() == transport::Control::Syn
                && context.transport_route().is_none()
            {
                return Err(());
            }
            if listening {
                entry.transport.set_hop_limit(if ipv6 {
                    state.options.ipv6_hop_limit
                } else {
                    state.options.hop_limit
                });
            }
            let hop_limit = entry.transport.hop_limit();
            response = entry
                .transport
                .process(&mut context, &ip, &verified)
                .map(|(mut ip, segment)| {
                    ip.set_hop_limit(hop_limit);
                    (ip, segment, (device, mark), key.cloned())
                });
            if listening && entry.transport.state() == State::SynReceived {
                entry.md5 = opening;
            }
            if listening && scoped && entry.transport.state() == State::SynReceived {
                // 只有真实 SYN 建立半连接才固定 zone；其他合法/无效控制段不能收窄 listener。
                entry.transport.set_bound_interface(device);
            }
            if entry.transmit_at > timestamp {
                entry.transmit_at = timestamp;
            }
            super::io::maintain_state(state);
            return Err(());
        }
        Ok(())
    });
    match matched {
        Err(()) => response,
        Ok(()) if raw || segment.control() == transport::Control::Rst => None,
        Ok(()) => {
            let scoped = matches!(ip.src_addr(),IpAddr::V6(address) if address.is_unicast_link_local())
                || matches!(ip.dst_addr(),IpAddr::V6(address) if address.is_unicast_link_local());
            let device = scoped.then_some(path.interface_index());
            let verified = segment.authenticate(ip.src_addr(), ip.dst_addr(), None)?;
            let (ip, segment) = transport::Socket::rst_reply(&ip, &verified);
            Some((ip, segment, (device, 0), None))
        }
    }
}

fn emit(
    interface: &mut NetworkInterface,
    route: Output,
    ip: transport::IpPacket,
    segment: transport::Segment<'_>,
    timestamp: Instant,
    key: Option<&transport::Md5Key>,
) -> Result<(), transport::OutputFailure> {
    let (source, destination) = (ip.src_addr(), ip.dst_addr());
    let ip_length = if source.is_ipv4() { 20 } else { 40 };
    let caps = interface.device.capabilities();
    let ethernet = caps.medium == Medium::Ethernet;
    let source_mac = interface.device.mac_address();
    let length = ip_length + segment.buffer_len();
    if length > route.mtu.min(interface.device.ip_mtu()) {
        return Err(transport::OutputFailure::PathUnavailable);
    }
    let token = interface
        .device
        .transmit(timestamp)
        .ok_or(transport::OutputFailure::LocalCongestion)?;
    let mut token = token;
    let link = if ethernet {
        let address = match (destination, route.neighbor) {
            (IpAddr::V4(destination), IpAddr::V4(neighbor)) => super::super::arp::resolve(
                &mut interface.arp,
                interface.state,
                destination,
                neighbor,
                timestamp,
            )
            .ok(),
            (_, IpAddr::V6(neighbor)) => interface.ipv6.as_mut().and_then(|owner| {
                owner
                    .resolve(neighbor, timestamp.total_millis().max(0) as u64)
                    .ok()
                    .flatten()
            }),
            _ => unreachable!("TCP path preserves family"),
        };
        Some(address.ok_or(transport::OutputFailure::PathUnavailable)?)
    } else {
        None
    };
    let offloaded = ethernet && !caps.checksum.tcp.tx();
    let mut metadata = smoltcp::phy::PacketMeta::default();
    metadata.interface_index = Some(route.logical_interface_index);
    metadata.tx_checksum = offloaded;
    token.set_meta(metadata);
    let link_length = if ethernet { 14 } else { 0 };
    let protocol = if source.is_ipv4() { 0x0800 } else { 0x86dd };
    let (_, failure) = token.consume_committed(link_length + length, protocol, |frame| {
        if let Some(destination) = link {
            packet_wire::ethernet::EthernetHeader {
                source: source_mac,
                destination,
                type_or_length: protocol,
                tags: &[],
            }
            .emit(frame)
            .expect("exact Ethernet TCP window");
        }
        let packet = &mut frame[link_length..];
        if let Some(key) = key {
            segment.emit_md5(
                &mut packet[ip_length..],
                source,
                destination,
                key,
                offloaded,
            )
        } else if offloaded {
            segment.emit_offloaded(&mut packet[ip_length..], source, destination)
        } else {
            segment.emit(&mut packet[ip_length..], source, destination)
        }
        .expect("exact TCP transport window");
        match (source, destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => Ipv4Header {
                source,
                destination,
                protocol: 6,
                hop_limit: ip.hop_limit().unwrap_or(route.hop_limit),
                dscp_ecn: 0,
                identification: 0,
                fragment: 0x4000,
                options: &[],
            }
            .emit(packet),
            (IpAddr::V6(source), IpAddr::V6(destination)) => Ipv6Header {
                source,
                destination,
                next_header: 6,
                hop_limit: ip.hop_limit().unwrap_or(route.hop_limit),
                traffic_class: 0,
                flow_label: 0,
            }
            .emit(packet),
            _ => unreachable!("TCP path preserves family"),
        }
        .expect("exact TCP IP window");
    });
    match failure {
        None => Ok(()),
        Some(device_model::network::NetworkError::WouldBlock) => {
            Err(transport::OutputFailure::LocalCongestion)
        }
        Some(_) => Err(transport::OutputFailure::PathUnavailable),
    }
}
impl NetworkStack {
    /// 真实设备/ARP 进展提前释放已有输出等待；没有设备或邻居状态的镜像。
    pub(in crate::inet) fn retry_tcp(&mut self, timestamp: Instant) {
        self.tcp_endpoints.for_each_mut(|_, state| {
            for entry in &mut state.handles {
                if entry.transmit_at > timestamp {
                    entry.transmit_at = timestamp;
                }
            }
        });
    }
    /// TCP 纯时间退休独立于路由；普通流输出只在实际等待到期且存在当前路径时驱动。
    pub(in crate::inet) fn tcp_due(&self, timestamp: Instant) -> bool {
        self.tcp_endpoints.values().any(|state| {
            state.handles.iter().any(|entry| {
                if entry
                    .transport
                    .maintenance_deadline()
                    .is_some_and(|deadline| deadline <= at(timestamp))
                {
                    return true;
                }
                if entry.transmit_at > timestamp {
                    return false;
                }
                let (Some(local), Some(peer)) = (
                    entry.transport.local_endpoint(),
                    entry.transport.remote_endpoint(),
                ) else {
                    return false;
                };
                let Ok(route) = self.ip_output(
                    local.addr,
                    InetAddress::new(peer.addr, peer.port),
                    entry.egress(state.options.mark),
                    timestamp,
                ) else {
                    return false;
                };
                let context = Path::new(
                    timestamp,
                    route.logical_interface_index,
                    Some((route.mtu, route.metrics)),
                    Some(local.addr),
                    &self.syn_sequence_key,
                );
                match entry.transport.poll_at(&context) {
                    transport::PollAt::Now => true,
                    transport::PollAt::Time(at_time) => at_time <= at(timestamp),
                    transport::PollAt::Ingress => false,
                }
            })
        })
    }
    /// 全域共享有界 TCP 输出预算，按 endpoint 轮转；只有完整设备提交后才推进协议序号。
    pub(in crate::inet) fn poll_tcp(
        &mut self,
        timestamp: Instant,
        first: usize,
        end: usize,
        rotation: usize,
    ) -> bool {
        let mut budget = super::super::NETWORK_RX_BUDGET;
        for _ in 0..self.tcp_endpoints.len() {
            let next = self
                .tcp_endpoints
                .successor(&self.tcp_cursor)
                .or_else(|| self.tcp_endpoints.first_key_value())
                .map(|(&id, _)| id);
            let Some(id) = next else {
                break;
            };
            self.tcp_cursor = id;
            let state = self
                .tcp_endpoints
                .get_mut(&id)
                .expect("TCP rotation lost endpoint");
            for entry in &mut state.handles {
                // transmit_at 限制时钟单独唤醒；实际 poll 已因输入/设备/应用活动发生时可重新尝试。
                // 不能仅凭旧次拥塞的等待时间忽略本轮已经恢复的设备容量。
                while let (Some(local), Some(peer)) = (
                    entry.transport.local_endpoint(),
                    entry.transport.remote_endpoint(),
                ) {
                    let route = super::super::route::output_for(
                        (
                            &self.routing,
                            &self.ipv6_routing,
                            self.selection_rules.local(),
                        ),
                        &self.interfaces,
                        &self.path_mtu,
                        local.addr,
                        InetAddress::new(peer.addr, peer.port),
                        entry.egress(state.options.mark),
                        timestamp,
                    );
                    let Ok(mut route) = route else {
                        entry.transport.output_unavailable(
                            at(timestamp),
                            transport::OutputFailure::PathUnavailable,
                        );
                        entry.transmit_at = timestamp + RETRY;
                        break;
                    };
                    let keys = entry
                        .passive
                        .as_ref()
                        .map_or(&state.md5_keys, |policy| &policy.md5_keys);
                    Binding::refresh(
                        &mut entry.md5,
                        keys,
                        peer.addr,
                        route.logical_interface_index,
                    );
                    if entry
                        .md5
                        .as_ref()
                        .is_some_and(|binding| !binding.permits(route.logical_interface_index))
                    {
                        entry.transport.output_unavailable(
                            at(timestamp),
                            transport::OutputFailure::PathUnavailable,
                        );
                        entry.transmit_at = timestamp + RETRY;
                        break;
                    }
                    let key = entry.md5.as_ref().map(|binding| binding.key.as_ref());
                    entry.transport.set_md5(key.is_some());
                    if peer.addr.is_ipv6()
                        && let Some(mtu) = state.options.ipv6_mtu
                    {
                        route.mtu = route.mtu.min(mtu as usize);
                    }
                    let count = self.interfaces.len();
                    let Some(position) = (first..end)
                        .map(|cursor| (rotation + cursor) % count)
                        .find(|position| self.interfaces[*position].index == route.interface_index)
                    else {
                        break;
                    };
                    if budget == 0 {
                        return true;
                    }
                    let mut attempted = false;
                    let mut context = Path::new(
                        timestamp,
                        route.logical_interface_index,
                        Some((route.mtu, route.metrics)),
                        Some(local.addr),
                        &self.syn_sequence_key,
                    );
                    let result = entry.transport.dispatch(&mut context, |_, (ip, segment)| {
                        attempted = true;
                        budget -= 1;
                        let recovery = route.recovery;
                        let result = emit(
                            &mut self.interfaces[position],
                            route,
                            ip,
                            segment,
                            timestamp,
                            key,
                        );
                        if result.is_ok() {
                            super::super::route::probe_gateway(
                                &mut self.interfaces,
                                recovery,
                                timestamp,
                            );
                        }
                        result
                    });
                    if result.is_err() {
                        entry.transmit_at = timestamp + RETRY;
                        // TCP 已为拥塞探测安排下一次尝试；外层防空转等待不能延后该协议时钟。
                        if let transport::PollAt::Time(deadline) = entry.transport.poll_at(&context)
                            && deadline > at(timestamp)
                        {
                            entry.transmit_at = entry
                                .transmit_at
                                .min(Instant::from_micros(deadline.total_micros()));
                        }
                        break;
                    }
                    if !attempted {
                        break;
                    }
                }
            }
        }
        false
    }
    /// 关闭的机制只移交一次 reset 字节；失败 best-effort 丢弃，不因此保留 endpoint/端口。
    pub(in crate::inet) fn queue_tcp_resets(&mut self, timestamp: Instant) {
        let replies = &mut self.route_replies;
        let routing = &self.routing;
        let interfaces = &self.interfaces;
        self.tcp_endpoints.for_each_mut(|_, state| {
            for entry in &mut state.handles {
                let Some((mut ip, mut segment)) = entry.transport.take_reset() else {
                    continue;
                };
                let Ok(route) = super::super::route::output_for(
                    (routing, &self.ipv6_routing, self.selection_rules.local()),
                    interfaces,
                    &self.path_mtu,
                    ip.src_addr(),
                    InetAddress::new(ip.dst_addr(), 0),
                    entry.egress(state.options.mark),
                    timestamp,
                ) else {
                    continue;
                };
                let keys = entry
                    .passive
                    .as_ref()
                    .map_or(&state.md5_keys, |policy| &policy.md5_keys);
                Binding::refresh(
                    &mut entry.md5,
                    keys,
                    ip.dst_addr(),
                    route.logical_interface_index,
                );
                if entry
                    .md5
                    .as_ref()
                    .is_some_and(|binding| !binding.permits(route.logical_interface_index))
                {
                    continue;
                }
                let key = entry.md5.as_ref().map(|binding| binding.key.as_ref());
                segment.md5 = key.is_some();
                ip.set_payload_len(segment.buffer_len());
                if !replies.enqueue_tcp(
                    ip,
                    segment,
                    entry.egress(state.options.mark),
                    route.interface_index,
                    timestamp,
                    key,
                ) {
                    interfaces
                        .iter()
                        .find(|interface| interface.index == route.interface_index)
                        .expect("TCP reset route lost device")
                        .device
                        .record_transmit_drop();
                }
            }
        });
    }
}

/// 核对引用的 TCP tuple 和在途序号。路径确认成功后允许本轮重新考虑发送。
pub(in crate::inet) fn match_too_big(
    endpoints: &mut FallibleMap<usize, TcpEndpointState>,
    addresses: (IpAddr, IpAddr),
    bytes: &[u8],
    interface: u32,
    mut accept: impl FnMut((Option<u32>, u32)) -> bool,
) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let local = IpEndpoint::new(addresses.0, u16::from_be_bytes([bytes[0], bytes[1]]));
    let remote = IpEndpoint::new(addresses.1, u16::from_be_bytes([bytes[2], bytes[3]]));
    let sequence = transport::Sequence(u32::from_be_bytes(
        bytes[4..8].try_into().expect("quoted TCP prefix"),
    ) as i32);
    let mut accepted = false;
    let _ = endpoints.try_for_each_mut::<()>(|_, state| {
        for entry in &mut state.handles {
            if entry
                .transport
                .accepts_error(local, remote, sequence, interface)
                && accept(entry.egress(state.options.mark))
            {
                accepted = true;
                entry.transmit_at = Instant::from_millis(0);
                return Err(());
            }
        }
        Ok(())
    });
    accepted
}
