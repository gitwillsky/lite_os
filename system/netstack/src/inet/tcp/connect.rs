//! 主动 TCP 打开与失败完成；实际端口、绑定和机制同一事务发布。
use super::*;

/// 通过唯一 interface context 启动 active TCP handshake。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `peer`: 远端双族 endpoint。
///
/// # Returns
///
/// SYN 提交后返回 `InProgress`；完成状态通过 readiness 观察。
///
/// # Errors
///
/// 返回标准地址、route、状态或 in-progress 错误。
pub(in crate::inet) fn connect(
    socket: &InetSocket,
    mut peer: InetAddress,
) -> Result<(), SocketError> {
    if peer.port() == 0 {
        return Err(SocketError::AddressNotAvailable);
    }
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let (bound, reuse_address, existing_lease) = match state.mode {
        TcpMode::Closed { bound, .. } => (bound, state.options.reuse_address, state.port_lease),
        TcpMode::Connecting { terminal: true, .. } => {
            return Err(finish_failed_connect(&mut network, id));
        }
        TcpMode::Connecting { .. } => return Err(SocketError::AlreadyInProgress),
        TcpMode::Connected(_) => return Err(SocketError::AlreadyConnected),
        TcpMode::Listening { .. } => return Err(SocketError::Invalid),
    };
    let family = state.family;
    if family == InetFamily::Ipv4 && peer.is_ipv6() {
        return Err(SocketError::AddressFamilyNotSupported);
    }
    if state.options.ipv6_only && peer.ip().to_canonical().is_ipv4() {
        return Err(if peer.is_ipv4() {
            SocketError::AddressFamilyNotSupported
        } else {
            SocketError::NetworkUnreachable
        });
    }
    let mut device = state.options.bound_to_device;
    if let InetAddress::V6(v6) = peer
        && v6.ip().is_unicast_link_local()
    {
        let zone = (v6.scope_id() != 0).then_some(v6.scope_id());
        if zone.is_some() && device.is_some() && zone != device {
            return Err(SocketError::Invalid);
        }
        device = Some(zone.or(device).ok_or(SocketError::Invalid)?);
        network
            .interface(device.expect("link zone"))
            .map_err(|_| SocketError::NoDevice)?;
    }
    let mut destination = peer.ip().to_canonical();
    if destination.is_unspecified() {
        destination = if destination.is_ipv4() {
            Ipv4Addr::LOCALHOST.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        };
        peer = present(family, InetAddress::new(destination, peer.port()), device);
    }
    if bound.is_some_and(|local| {
        !local.ip().is_unspecified() && local.ip().to_canonical().is_ipv4() != destination.is_ipv4()
    }) {
        return Err(SocketError::NetworkUnreachable);
    }
    if destination.is_multicast() {
        return Err(SocketError::NetworkUnreachable);
    }
    let source = bound
        .map(|local| local.ip().to_canonical())
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or_else(|| {
            if destination.is_ipv4() {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv6Addr::UNSPECIFIED.into()
            }
        });
    let timestamp = super::super::now();
    let mut route = network.ip_output(
        source,
        InetAddress::new(destination, peer.port()),
        (device, state.options.mark),
        timestamp,
    )?;
    if route.source.is_unspecified() {
        return Err(SocketError::NetworkUnreachable);
    }
    if route.kind == network_protocol::RouteKind::Broadcast {
        return Err(SocketError::NetworkUnreachable);
    }
    if destination.is_ipv6()
        && let Some(mtu) = network.tcp_endpoints[&id].options.ipv6_mtu
    {
        route.mtu = route.mtu.min(mtu as usize);
    }
    let metrics = route.metrics;
    lifecycle::prepare_closed_transport(&mut network, id)?;
    let exact = present(family, InetAddress::new(route.source, 0), device);
    let binding = PortBinding::new(exact.ip(), network.tcp_endpoints[&id].options.ipv6_only)
        .map_err(port_error)?
        .with_interface(device);
    let prepared = existing_lease
        .map(|lease| network.tcp_ports.prepare_readdress(lease, binding))
        .transpose()
        .map_err(port_error)?;
    let new_lease = if bound.is_none() {
        Some(
            network
                .tcp_ports
                .acquire_ephemeral(binding, reuse_address)
                .map_err(port_error)?,
        )
    } else {
        None
    };
    let local =
        bound.unwrap_or_else(|| unspecified(family, new_lease.expect("unbound TCP port").port()));
    let NetworkStack {
        tcp_endpoints,
        syn_sequence_key,
        ..
    } = &mut *network;
    let mut context = Path::new(
        timestamp,
        route.logical_interface_index,
        Some((route.mtu, metrics)),
        Some(route.source),
        syn_sequence_key,
    );
    let state = tcp_endpoints.get_mut(&id).expect("TCP open disappeared");
    Binding::refresh(
        &mut state.handles[0].md5,
        &state.md5_keys,
        destination,
        route.logical_interface_index,
    );
    {
        let entry = &mut state.handles[0];
        entry.transport.set_md5(entry.md5.is_some());
    }
    state.handles[0].transport.set_bound_interface(device);
    if state.handles[0]
        .transport
        .connect(
            &mut context,
            IpEndpoint::new(destination, peer.port()),
            IpListenEndpoint {
                addr: Some(route.source),
                port: local.port(),
            },
        )
        .is_err()
    {
        state.handles[0]
            .transport
            .set_bound_interface(state.options.bound_to_device);
        if let Some(lease) = new_lease {
            network.tcp_ports.release(lease);
        }
        return Err(SocketError::AddressNotAvailable);
    }
    if destination.is_ipv6() {
        state.handles[0]
            .transport
            .set_hop_limit(state.options.ipv6_hop_limit);
    }
    let lease = new_lease.unwrap_or_else(|| {
        network
            .tcp_ports
            .commit_readdress(prepared.expect("bound TCP prepared address"))
    });
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP open disappeared");
    state.mode = TcpMode::Connecting {
        bound: local,
        local: present(family, InetAddress::new(route.source, local.port()), device),
        terminal: false,
        shutdown_read: false,
        shutdown_write: false,
    };
    state.options.bound_to_device = device;
    state.pending_error = None;
    state.port_lease = Some(lease);
    drop(network);
    crate::worker::request_poll();
    Err(SocketError::InProgress)
}

/// 在 OFD writable/error wakeup 后解析 active-connect 完成状态。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// 仅在 established 后返回 unit。
///
/// # Errors
///
/// 返回 in-progress、refusal 或无效状态错误。
pub(in crate::inet) fn connection_result(socket: &InetSocket) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    match state.mode {
        TcpMode::Connecting { terminal: true, .. } => Err(finish_failed_connect(&mut network, id)),
        TcpMode::Connecting { .. } => Err(SocketError::InProgress),
        TcpMode::Connected(_) => {
            let phase = state.handles[0].transport.state();
            if phase == State::Closed {
                Err(finish_failed_connect(&mut network, id))
            } else {
                Ok(())
            }
        }
        _ => Err(SocketError::NotConnected),
    }
}

/// __inet_stream_connect 的失败完成：消费 error、恢复 user bind 与未连接 read policy。
fn finish_failed_connect(network: &mut NetworkStack, id: usize) -> SocketError {
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("failed connect lost its actual endpoint while stack is held");
    let bound = match state.mode {
        TcpMode::Connecting { bound, .. } => bound,
        TcpMode::Connected(connection) => connection.bound,
        _ => unreachable!("failed connect requires an actual connection attempt"),
    };
    let error = state
        .pending_error
        .take()
        .unwrap_or(SocketError::ConnectionAborted);
    state.mode = TcpMode::Closed {
        bound: Some(bound),
        shutdown_read: false,
    };
    if let Some(lease) = state.port_lease {
        state.port_lease = Some(
            network.tcp_ports.restore_bound_address(
                lease,
                PortBinding::new(bound.ip(), state.options.ipv6_only)
                    .expect("existing TCP binding is valid")
                    .with_interface(state.options.bound_to_device),
            ),
        );
    }
    error
}
