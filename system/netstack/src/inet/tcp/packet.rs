//! TCP 的实际收发接入；连接直接属于 endpoint，路径与邻居只借本 NET 的唯一 owner。
use super::super::{interface::NetworkInterface, route::Output};
use super::*;
use alloc::sync::Arc;
use core::net::IpAddr;
use packet_wire::{ipv4::Ipv4Header, ipv6::Ipv6Header, tcp::TcpPacket};
use ::tcp::time::Duration;
type Reply = (
    transport::IpPacket,
    transport::Segment<'static>,
    (Option<u32>, u32),
    ReplyKey,
);

pub(in crate::inet) enum ReplyKey {
    None,
    Md5(Arc<transport::Md5Key>),
    Ao(Arc<transport::AoTrafficKey>, (u8, u8), u32),
}

impl ReplyKey {
    pub(in crate::inet) fn signing_key(&self) -> transport::SigningKey<'_> {
        match self {
            Self::None => transport::SigningKey::None,
            Self::Md5(key) => transport::SigningKey::Md5(key),
            Self::Ao(key, ids, sne) => transport::SigningKey::Ao {
                key,
                ids: *ids,
                sne: *sne,
            },
        }
    }
}
const RETRY: Duration = Duration::from_millis(1000);

impl TcpHandle {
    /// 半连接的策略由实际子连接持有，监听端后续修改仅影响新连接。
    fn egress(&self, endpoint_mark: u32) -> (Option<u32>, u32) {
        (
            self.transport
                .bound_interface()
                .or_else(|| self.ao.as_ref().and_then(|ao| ao.output_scope())),
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
    packet: crate::inet::ingress::Ipv4Ingress<'_>,
    timestamp: Instant,
    key: &SequenceKey,
    marked_route: impl Fn(Option<u32>, u32) -> Option<(usize, network_protocol::RouteMetrics)>,
) -> Option<Reply> {
    let source = packet.ip.source();
    let destination = packet.ip.destination();
    if packet.multicast
        || source.is_unspecified()
        || source.is_multicast()
        || source.is_broadcast()
        || destination.is_unspecified()
    {
        return None;
    }
    let parsed = if packet.checksum_valid {
        TcpPacket::parse_header(packet.ip.payload())
    } else {
        TcpPacket::parse(packet.ip.payload(), source.into(), destination.into())
    }
    .ok()?;
    let segment = transport::IncomingSegment::capture(parsed).ok()?;
    let ip = transport::IpPacket::new(
        source.into(),
        destination.into(),
        6,
        packet.ip.payload().len(),
        Some(packet.ip.hop_limit()),
    );
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
                return marked_route(device, mark);
            }
            match device {
                Some(_) => packet.facts.bound_reverse_route,
                None => packet.facts.reverse_route,
            }
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
    let timestamp = path.now();
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
            let interface = path.interface_index();
            // 临时候选只在验签成功且 TCP 接纳 SYN 后发表。
            let mut opening = None;
            let mut opening_ao = None;
            let mut md5 = None;
            let verified = if let Some(ao) = &mut entry.ao {
                let counters = state
                    .ao_counters
                    .as_deref_mut()
                    .expect("AO connection without counters");
                ao.authenticate(segment, interface, counters).ok_or(())?
            } else if listening && ao::MasterKey::matches(&state.ao_keys, ip.src_addr(), interface)
            {
                let counters = state
                    .ao_counters
                    .as_deref_mut()
                    .expect("AO listener without counters");
                let (connection, verified) =
                    AoConnection::open_passive(&state.ao_keys, ip, segment, interface, counters)
                        .ok_or(())?;
                opening_ao = Some(connection);
                verified
            } else if segment.ao_ids().is_some() {
                if let Some(counters) = state.ao_counters.as_deref_mut() {
                    counters.key_not_found = counters.key_not_found.wrapping_add(1);
                }
                return Err(());
            } else {
                let keys = entry
                    .passive
                    .as_ref()
                    .map_or(&state.md5_keys, |policy| &policy.md5_keys);
                let binding = if listening {
                    &mut opening
                } else {
                    &mut entry.md5
                };
                Binding::refresh(binding, keys, ip.src_addr(), interface);
                if binding
                    .as_ref()
                    .is_some_and(|binding| !binding.permits(interface))
                {
                    return Err(());
                }
                md5 = binding.as_ref().map(|binding| &binding.key);
                segment
                    .authenticate(ip.src_addr(), ip.dst_addr(), md5.map(Arc::as_ref))
                    .ok_or(())?
            };
            let mut context = path.with_route(reverse_route(device, mark));
            if listening
                && segment.control() == transport::Control::Syn
                && context.transport_route().is_none()
            {
                return Err(());
            }
            entry.transport.set_authentication(verified.authentication);
            if listening {
                entry.transport.set_hop_limit(if ipv6 {
                    state.options.ipv6_hop_limit
                } else {
                    state.options.hop_limit
                });
            }
            let hop_limit = entry.transport.hop_limit();
            let packet = entry.transport.process(&mut context, &ip, &verified);
            let mut reply_key = if packet.is_some() {
                md5.cloned().map_or(ReplyKey::None, ReplyKey::Md5)
            } else {
                ReplyKey::None
            };
            if listening && entry.transport.state() == State::SynReceived {
                if let Some(connection) = &mut opening_ao {
                    connection.start(&entry.transport);
                }
                entry.ao = opening_ao;
                entry.md5 = opening;
            }
            let mut reply_device = device;
            let mut packet = packet;
            if let Some(ao) = &mut entry.ao {
                ao.advance(&entry.transport, &verified);
                if let Some((_, segment)) = &packet {
                    reply_device = reply_device.or_else(|| ao.output_scope());
                    if let Some((key, ids, sne)) = ao.signing_key(segment, interface) {
                        reply_key = ReplyKey::Ao(key.clone(), ids, sne);
                    } else {
                        packet = None;
                    }
                }
            }
            response = packet.map(|(mut ip, segment)| {
                ip.set_hop_limit(hop_limit);
                (ip, segment, (reply_device, mark), reply_key)
            });
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
            Some((ip, segment, (device, 0), ReplyKey::None))
        }
    }
}

fn emit(
    interface: &mut NetworkInterface,
    route: Output,
    ip: transport::IpPacket,
    segment: transport::Segment<'_>,
    timestamp: Instant,
    key: transport::SigningKey<'_>,
) -> Result<(), transport::OutputFailure> {
    let (source, destination) = (ip.src_addr(), ip.dst_addr());
    let ip_length = if source.is_ipv4() { 20 } else { 40 };
    let ethernet = matches!(interface.device, super::super::device::ProtocolDevice::Ethernet(_));
    let source_mac = interface.device.mac_address();
    let length = ip_length + segment.buffer_len();
    if length > route.mtu.min(interface.device.ip_mtu()) {
        return Err(transport::OutputFailure::PathUnavailable);
    }
    let mut token = interface.device.reserve(|_| 0).map_err(|error| match error {
        device_model::network::NetworkError::WouldBlock => transport::OutputFailure::LocalCongestion,
        _ => transport::OutputFailure::PathUnavailable,
    })?;
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
    let offloaded = interface.device.transmit_checksum();
    token.set_delivery(route.logical_interface_index, offloaded);
    let link_length = if ethernet { 14 } else { 0 };
    let protocol = if source.is_ipv4() { 0x0800 } else { 0x86dd };
    let submitted = token.consume_committed(link_length + length, protocol, |frame| {
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
        segment
            .emit_with_authentication(
                &mut packet[ip_length..],
                (source, destination),
                key,
                offloaded,
            )
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
        true
    });
    match submitted {
        Ok(()) => Ok(()),
        Err(device_model::network::NetworkError::WouldBlock) => {
            Err(transport::OutputFailure::LocalCongestion)
        }
        Err(_) => Err(transport::OutputFailure::PathUnavailable),
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
    pub(in crate::inet) fn tcp_deadline(&self, timestamp: Instant) -> Option<Instant> {
        // 实际推进时钟以毫秒为单位，向上取整避免在截止前反复立即唤醒。
        let wake = |time: transport::time::Instant| {
            Instant::from_millis(time.total_micros().saturating_add(999) / 1000)
        };
        self.tcp_endpoints
            .values()
            .flat_map(|state| {
                state.handles.iter().flat_map(move |entry| {
                    let maintenance = entry
                        .transport
                        .maintenance_deadline(entry.transmit_at)
                        .map(wake);
                    let transmit = (|| {
                        if entry.transport.output_interest() == transport::PollAt::Ingress {
                            return None;
                        }
                        let local = entry.transport.local_endpoint()?;
                        let peer = entry.transport.remote_endpoint()?;
                        let route = self
                            .ip_output(
                                local.addr,
                                InetAddress::new(peer.addr, peer.port),
                                entry.egress(state.options.mark),
                                timestamp,
                            )
                            .ok()?;
                        let context = Path::new(
                            timestamp,
                            route.logical_interface_index,
                            Some((route.mtu, route.metrics)),
                            Some(local.addr),
                            &self.syn_sequence_key,
                        );
                        match entry.transport.poll_at(&context) {
                            transport::PollAt::Now => Some(timestamp.max(entry.transmit_at)),
                            transport::PollAt::Time(at_time) => {
                                Some(wake(at_time).max(entry.transmit_at))
                            }
                            transport::PollAt::Ingress => None,
                        }
                    })();
                    [maintenance, transmit].into_iter().flatten()
                })
            })
            .min()
    }
    /// 全域共享有界 TCP 输出预算，按 endpoint 轮转；只有完整设备提交后才推进协议序号。
    pub(in crate::inet) fn poll_tcp(
        &mut self,
        timestamp: Instant,
        first: usize,
        end: usize,
        rotation: usize,
        work_until: u64,
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
            let state = self
                .tcp_endpoints
                .get_mut(&id)
                .expect("TCP rotation lost endpoint");
            let handle_count = state.handles.len();
            let first_handle = state.output_handle;
            let before = budget;
            let mut exhausted = false;
            'handles: for offset in 0..handle_count {
                let index = (first_handle + offset) % handle_count;
                let entry = &mut state.handles[index];
                // transmit_at 限制时钟单独唤醒；实际 poll 已因输入/设备/应用活动发生时可重新尝试。
                // 不能仅凭旧次拥塞的等待时间忽略本轮已经恢复的设备容量。
                while let (Some(local), Some(peer)) = (
                    entry.transport.local_endpoint(),
                    entry.transport.remote_endpoint(),
                ) {
                    match entry.transport.output_interest() {
                        transport::PollAt::Ingress => break,
                        transport::PollAt::Time(deadline) if deadline > timestamp => break,
                        _ => {}
                    }
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
                    let position = if let Ok(route) = &route {
                        let count = self.interfaces.len();
                        let Some(position) = (first..end)
                            .map(|cursor| (rotation + cursor) % count)
                            .find(|position| self.interfaces[*position].index == route.interface_index)
                        else {
                            break;
                        };
                        Some(position)
                    } else {
                        None
                    };
                    if !super::super::poll::spend_output_attempt(&mut budget, work_until) {
                        exhausted = true;
                        break 'handles;
                    }
                    self.tcp_cursor = id;
                    state.output_handle = (index + 1) % handle_count;
                    let Ok(mut route) = route else {
                        entry.transport.output_unavailable(
                            timestamp,
                            transport::OutputFailure::PathUnavailable,
                        );
                        entry.transmit_at = timestamp + RETRY;
                        break;
                    };
                    let keys = entry
                        .passive
                        .as_ref()
                        .map_or(&state.md5_keys, |policy| &policy.md5_keys);
                    if entry.ao.is_none() {
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
                                timestamp,
                                transport::OutputFailure::PathUnavailable,
                            );
                            entry.transmit_at = timestamp + RETRY;
                            break;
                        }
                    }
                    let key = entry.md5.as_ref().map(|binding| binding.key.as_ref());
                    entry.transport.set_authentication(if entry.ao.is_some() {
                        transport::Authentication::Ao
                    } else if key.is_some() {
                        transport::Authentication::Md5
                    } else {
                        transport::Authentication::None
                    });
                    if peer.addr.is_ipv6()
                        && let Some(mtu) = state.options.ipv6_mtu
                    {
                        route.mtu = route.mtu.min(mtu as usize);
                    }
                    let position = position.expect("active route has a matching output window");
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
                        let recovery = route.recovery;
                        let key = if let Some(ao) = &mut entry.ao {
                            let (key, ids, sne) = ao
                                .signing_key(&segment, route.logical_interface_index)
                                .ok_or(transport::OutputFailure::PathUnavailable)?;
                            transport::SigningKey::Ao { key, ids, sne }
                        } else {
                            key.map_or(transport::SigningKey::None, transport::SigningKey::Md5)
                        };
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
                            && deadline > timestamp
                        {
                            entry.transmit_at = entry
                                .transmit_at
                                .min(deadline);
                        }
                        break;
                    }
                    if !attempted {
                        break;
                    }
                }
            }
            // 时钟和接收已观察各自的变更。这里只观察本轮实际尝试输出的状态。
            if budget != before {
                super::io::maintain_state(state);
            }
            // 无需可用网卡，也无需重建连接目录。
            if state.orphan_finished() {
                self.retire_tcp_orphan(id);
            }
            if exhausted {
                return true;
            }
            self.tcp_cursor = id;
        }
        false
    }
    /// 先推进 transport 时钟，再移交产生的 RST。无路由或网卡背压不阻止时钟及连接退休。
    pub(in crate::inet) fn maintain_tcp_clocks(&mut self, timestamp: Instant) {
        let (endpoints, mut queue_resets) = self.tcp_reset_output(timestamp);
        endpoints.for_each_mut(|_, state| {
            for entry in &mut state.handles {
                entry.transport.maintain(timestamp);
            }
            super::io::maintain_state(state);
            queue_resets(state);
        });
    }

    /// 在 `id` 的机制释放或复用前，消费该端点待发的 RST。
    pub(super) fn queue_tcp_resets(&mut self, id: usize, timestamp: Instant) {
        let (endpoints, mut queue_resets) = self.tcp_reset_output(timestamp);
        queue_resets(endpoints.get_mut(&id).expect("live TCP endpoint"));
    }

    // 拆借原有路由与报文队列。RST 准入失败时直接丢弃，不恢复 transport 或端口租约。
    fn tcp_reset_output(
        &mut self,
        timestamp: Instant,
    ) -> (
        &mut FallibleMap<usize, TcpEndpointState>,
        impl FnMut(&mut TcpEndpointState) + '_,
    ) {
        let replies = &mut self.route_replies;
        let routing = &self.routing;
        let ipv6_routing = &self.ipv6_routing;
        let rules = self.selection_rules.local();
        let path_mtu = &self.path_mtu;
        let interfaces = &self.interfaces;
        let queue_resets = move |state: &mut TcpEndpointState| {
            for entry in &mut state.handles {
                let Some((mut ip, mut segment)) = entry.transport.take_reset() else {
                    continue;
                };
                let egress = entry.egress(state.options.mark);
                let mut ao = entry.ao.take();
                let Ok(route) = super::super::route::output_for(
                    (routing, ipv6_routing, rules),
                    interfaces,
                    path_mtu,
                    ip.src_addr(),
                    InetAddress::new(ip.dst_addr(), 0),
                    egress,
                    timestamp,
                ) else {
                    continue;
                };
                let keys = entry
                    .passive
                    .as_ref()
                    .map_or(&state.md5_keys, |policy| &policy.md5_keys);
                let key = if let Some(ao) = &mut ao {
                    let Some((key, ids, sne)) =
                        ao.signing_key(&segment, route.logical_interface_index)
                    else {
                        continue;
                    };
                    segment.authentication = transport::Authentication::Ao;
                    transport::SigningKey::Ao { key, ids, sne }
                } else {
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
                    if let Some(binding) = &entry.md5 {
                        segment.authentication = transport::Authentication::Md5;
                        transport::SigningKey::Md5(&binding.key)
                    } else {
                        segment.authentication = transport::Authentication::None;
                        transport::SigningKey::None
                    }
                };
                ip.set_payload_len(segment.buffer_len());
                if !replies.enqueue_tcp(ip, segment, egress, route.interface_index, timestamp, key)
                {
                    interfaces
                        .iter()
                        .find(|interface| interface.index == route.interface_index)
                        .expect("TCP reset route lost device")
                        .device
                        .record_transmit_drop();
                }
            }
        };
        (&mut self.tcp_endpoints, queue_resets)
    }
}

/// 核对引用的 TCP tuple 和在途序号。路径确认成功后允许本轮重新考虑发送。
/// 返回 true 时，此引用属于已忽略错误的 AO 流，不能经 RAW 反馈重新更新同一路径。
pub(in crate::inet) fn receive_icmp_error(
    endpoints: &mut FallibleMap<usize, TcpEndpointState>,
    report: &super::super::icmp_error::Report<'_>,
    mut accept: impl FnMut((Option<u32>, u32)) -> bool,
) -> bool {
    let bytes = report.transport;
    if bytes.len() < 8 {
        return false;
    }
    let local = IpEndpoint::new(report.source, u16::from_be_bytes([bytes[0], bytes[1]]));
    let remote = IpEndpoint::new(report.destination, u16::from_be_bytes([bytes[2], bytes[3]]));
    let sequence = transport::Sequence(u32::from_be_bytes(
        bytes[4..8].try_into().expect("quoted TCP prefix"),
    ) as i32);
    endpoints.try_for_each_mut(|_, state| {
        for entry in &mut state.handles {
            if !entry
                .transport
                .accepts_error(local, remote, sequence, report.interface)
            {
                continue;
            }
            // RFC 5925 §7.8: an unauthenticated hard error must not alter a
            // synchronized AO flow, including IPv4 fragmentation-needed.
            if report.ao_hard_error()
                && entry.ao.is_some()
                && !matches!(entry.transport.state(), State::SynSent | State::SynReceived)
            {
                let counters = state
                    .ao_counters
                    .as_deref_mut()
                    .expect("AO connection without counters");
                counters.ignored_icmp = counters.ignored_icmp.wrapping_add(1);
                return Err(true);
            }
            if report.too_big() && accept(entry.egress(state.options.mark)) {
                entry.transmit_at = Instant::from_millis(0);
                return Err(false);
            }
        }
        Ok(())
    }) == Err(true)
}
