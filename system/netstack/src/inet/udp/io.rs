//! UDP 原子收发；载荷复制在 NET owner 外完成，元数据与队列仍由同一 endpoint 拥有。
use super::*;
use network_protocol::InternetSendPacketInfo;

/// 向显式目标或默认 peer 发送完整 datagram；发送方向存储首次使用时在 loan 内准备。
/// # Errors
/// 长度/MTU、路由、授权、shutdown、队列背压或 OOM；失败不发布半条数据报。
pub(in crate::inet) fn send(
    endpoint: &InetSocket,
    id: u64,
    input: &[u8],
    target: Option<InetAddress>,
    packet_info: &[InternetSendPacketInfo],
) -> Result<usize, SocketError> {
    if input.len() > network_protocol::MAX_IPV6_UDP_BYTES {
        return Err(SocketError::MessageTooLarge);
    }
    let stack = endpoint.stack()?;
    let written = stack.with_payload_loan(
        |network| {
            if let Some(error) = network
                .udp_endpoints
                .get_mut(&id)
                .expect("live UDP disappeared")
                .errors
                .pending
                .take()
            {
                return Err(error);
            }
            network.ensure_udp_bound(id)?;
            let state = &network.udp_endpoints[&id];
            let peer = state.destination(
                target
                    .or(state.peer)
                    .ok_or(SocketError::DestinationRequired)?,
            )?;
            if peer.port() == 0 || peer.ip().is_unspecified() {
                return Err(SocketError::AddressNotAvailable);
            }
            let options = state.options;
            let (source, outgoing) = state.send_packet_info(network, peer, packet_info)?;
            let peer = wire_address(peer);
            let route = network.ip_output(
                source,
                peer,
                (outgoing, options.mark),
                super::super::now(),
            )?;
            let header = if peer.is_ipv4() { 28 } else { 48 };
            let mtu = if peer.is_ipv4() {
                if options.ipv4_mtu_discovery >= 3 {
                    network.interface(route.interface_index)?.device.ip_mtu()
                } else {
                    route.mtu
                }
            } else {
                options
                    .ipv6_mtu
                    .map_or(route.mtu, |mtu| route.mtu.min(mtu as usize))
            };
            let fragmentable = if peer.is_ipv4() {
                matches!(options.ipv4_mtu_discovery, 0 | 1 | 5)
            } else {
                !options.ipv6_dont_fragment
            };
            if peer.is_ipv4() && input.len() > network_protocol::MAX_IPV4_UDP_BYTES {
                return Err(SocketError::MessageTooLarge);
            }
            if !fragmentable && input.len() > mtu.saturating_sub(header) {
                let destination = state.present(peer, Some(route.logical_interface_index));
                let state = network.udp_endpoints.get_mut(&id).expect("live UDP");
                if state.errors.local_mtu(destination, mtu) {
                    state.notification_pending = true;
                    crate::worker::request_poll();
                }
                return Err(SocketError::MessageTooLarge);
            }
            if route.kind == network_protocol::RouteKind::Broadcast && !options.broadcast {
                return Err(SocketError::PermissionDenied);
            }
            if state.shutdown & SEND_SHUTDOWN != 0 {
                return Err(SocketError::BrokenPipe);
            }
            let mut metadata = Transmit {
                source: InetAddress::new(route.source, state.local.port()),
                destination: peer,
                interface: route.logical_interface_index,
                mark: options.mark,
                outgoing: (if peer.ip().is_ipv4() || peer.ip().is_multicast()
                    || matches!(route.source, IpAddr::V6(address) if address.is_unicast_link_local())
                {
                    Some(route.logical_interface_index)
                } else {
                    outgoing
                }).and_then(core::num::NonZeroU32::new),
                multicast_loop: options.multicast_loop(peer.ip()),
                hop_limit: options.datagram_hop_limit(peer.ip(), route.hop_limit),
                fragment_offset: 0,
                identification: None,
                ipv4_mtu_discovery: options.ipv4_mtu_discovery,
                ipv4_df: matches!(options.ipv4_mtu_discovery, 2 | 3)
                    || options.ipv4_mtu_discovery == 1 && input.len() + header <= mtu,
                fragment_checksum: None,
                ipv6_dont_fragment: options.ipv6_dont_fragment,
                ipv6_mtu: options.ipv6_mtu,
            };
            if !metadata.ipv4_df
                && let (IpAddr::V4(source), IpAddr::V4(destination)) = (route.source, peer.ip())
            {
                metadata.identification = Some(network.identification.reserve(
                    source,
                    destination,
                    17,
                    super::super::now(),
                )?);
            }
            if input.len() + header > mtu
                && let (IpAddr::V6(source), IpAddr::V6(destination)) = (route.source, peer.ip())
            {
                metadata.identification = Some(network.identification.reserve_ipv6(
                    source,
                    destination,
                    super::super::now(),
                )?);
            }
            let queue = core::mem::replace(
                &mut network
                    .udp_endpoints
                    .get_mut(&id)
                    .expect("live UDP disappeared")
                    .transmit,
                Queue::new(),
            );
            Ok((queue, metadata))
        },
        |(queue, metadata)| {
            queue.prepare(128 * 1024)?;
            queue.push(*metadata, input)?;
            // 发布后的记录是唯一保留者；失败时 metadata 仍负责回收。
            metadata.identification = None;
            Ok(input.len())
        },
        |network, (queue, metadata)| {
            if let Some(lease) = metadata.identification {
                network.identification.release(lease, super::super::now());
            }
            let state = network
                .udp_endpoints
                .get_mut(&id)
                .expect("borrowed UDP disappeared");
            state.transmit = queue;
            state.transmit_at = super::super::now();
        },
    )?;
    crate::worker::request_poll();
    Ok(written)
}

/// 接收或 PEEK 一条 datagram，保留原始长度和入队时冻结的来源/packet info。
/// # Errors
/// 空且未 read-shutdown 返回 Again；队列空且 read-shutdown 返回无 sockaddr 的 EOF。
pub(in crate::inet) fn receive(
    endpoint: &InetSocket,
    id: u64,
    output: &mut ReceiveBuffer,
    peek: bool,
) -> Result<
    (
        usize,
        usize,
        Option<InetAddress>,
        Option<InternetPacketInfo>,
    ),
    SocketError,
> {
    endpoint.stack()?.with_payload_loan(
        |network| {
            let state = network
                .udp_endpoints
                .get_mut(&id)
                .expect("live UDP disappeared");
            if let Some(error) = state.errors.pending.take() {
                return Err(error);
            }
            Ok((
                core::mem::replace(&mut state.receive, Queue::new()),
                state.shutdown & RECEIVE_SHUTDOWN != 0,
                state.options,
            ))
        },
        |(queue, read_closed, options)| {
            let Some((metadata, payload)) = queue.peek() else {
                return if *read_closed {
                    Ok((0, 0, None, None))
                } else {
                    Err(SocketError::Again)
                };
            };
            let result = (
                output.append(payload),
                payload.len(),
                Some(metadata.source),
                options.receive_packet_info(metadata.packet_info),
            );
            if !peek {
                queue.pop();
            }
            Ok(result)
        },
        |network, (queue, _, _)| {
            network
                .udp_endpoints
                .get_mut(&id)
                .expect("borrowed UDP disappeared")
                .receive = queue;
        },
    )
}
