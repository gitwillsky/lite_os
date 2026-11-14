use super::*;

/// 同一 CLOSED endpoint 内完整准备流缓冲；OOM 保留原绑定与机制，旧 reset 只由报文队列持有。
pub(super) fn prepare_closed_transport(
    network: &mut NetworkStack,
    id: usize,
) -> Result<(), SocketError> {
    assert!(matches!(
        network.tcp_endpoints[&id].mode,
        TcpMode::Closed { .. }
    ));
    let send_mss = network.tcp_endpoints[&id].handles[0].transport.send_mss();
    network.queue_tcp_resets(id, super::super::now());
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("closed endpoint disappeared");
    state.handles.truncate(1);
    if state.handles[0].transport.recv_capacity() == 0 {
        let mut prepared = super::storage::allocate_socket()?;
        prepared.set_send_mss(send_mss);
        state.handles[0].transport = prepared;
    }
    state.options.apply_tcp(&mut state.handles[0].transport);
    Ok(())
}

/// 取消实际连接/监听并恢复原绑定；只保留待交付 reset 所需的机制，报文取得副本后不借端口。
pub(super) fn disconnect_locked(network: &mut NetworkStack, id: usize) {
    let NetworkStack {
        tcp_endpoints,
        tcp_ports,
        ..
    } = network;
    let state = tcp_endpoints
        .get_mut(&id)
        .expect("disconnect lost TCP endpoint");
    let (bound, listener) = match state.mode {
        TcpMode::Closed { bound, .. } => (bound, false),
        TcpMode::Listening { endpoint, .. } => (Some(endpoint), true),
        TcpMode::Connecting { bound, .. } => (Some(bound), false),
        TcpMode::Connected(connection) => (Some(connection.bound), false),
    };
    let send_mss = match state.mode {
        TcpMode::Listening { send_mss, .. } => send_mss,
        _ => state.handles[0].transport.send_mss(),
    };
    state.options.user_mss = None;
    if !listener && let Some(entry) = state.handles.first() {
        let tcp = &entry.transport;
        if matches!(
            tcp.state(),
            State::SynSent
                | State::SynReceived
                | State::Established
                | State::CloseWait
                | State::FinWait1
                | State::FinWait2
        ) || matches!(tcp.state(), State::Closing | State::LastAck) && tcp.send_queue() != 0
        {
            state.pending_error = Some(SocketError::ConnectionReset);
        }
    }
    let mut retained = false;
    state.handles.retain_mut(|entry| {
        let tcp = &mut entry.transport;
        let needs_reset = matches!(
            tcp.state(),
            State::SynReceived
                | State::Established
                | State::CloseWait
                | State::FinWait1
                | State::FinWait2
        ) || matches!(tcp.state(), State::Closing | State::LastAck)
            && tcp.send_queue() != 0
            || tcp.state() == State::Closed && tcp.remote_endpoint().is_some();
        if let Some(policy) = entry.passive.take()
            && let Some(lease) = policy.port_lease
        {
            tcp_ports.release(lease);
        }
        tcp.set_user_mss(None);
        tcp.set_send_mss(send_mss);
        if needs_reset {
            tcp.abort();
            retained = true;
            true
        } else if !retained {
            *tcp = placeholder_socket();
            tcp.set_send_mss(send_mss);
            entry.ao = None;
            entry.md5 = None;
            retained = true;
            true
        } else {
            false
        }
    });
    if let Some(mut lease) = state.port_lease {
        if listener {
            lease = tcp_ports.stop_listener(lease);
        }
        state.port_lease = Some(
            tcp_ports.restore_bound_address(
                lease,
                PortBinding::new(
                    bound.unwrap_or_else(|| unspecified(state.family, 0)).ip(),
                    state.options.ipv6_only,
                )
                .expect("existing TCP binding remains valid")
                .with_interface(state.options.bound_to_device),
            ),
        );
    }
    state.mode = TcpMode::Closed {
        bound,
        shutdown_read: false,
    };
    network.queue_tcp_resets(id, super::super::now());
    // reset 字节已独立移交或按预算丢弃；关闭的监听槽不应继续占用连接容量和流缓冲。
    network
        .tcp_endpoints
        .get_mut(&id)
        .expect("disconnect lost closed endpoint")
        .handles
        .truncate(1);
}

/// AF_UNSPEC 取消连接或监听；状态与端口仍归同一 endpoint。
pub(in crate::inet) fn disconnect(socket: &InetSocket) -> Result<(), SocketError> {
    let mut network = socket.stack()?.lock()?;
    let id = endpoint_id(socket);
    if !network.tcp_endpoints.contains_key(&id) {
        return Err(SocketError::NotConnected);
    }
    disconnect_locked(&mut network, id);
    drop(network);
    crate::worker::request_poll(socket.namespace.id());
    Ok(())
}

/// 最后一个描述符移交关闭责任；完整读取的活动连接保留 FIN/TIME_WAIT，未读流立即 abort。
pub(in crate::inet) fn drop_endpoint(network: &mut NetworkStack, id: usize) {
    let Some(mode) = network.tcp_endpoints.get(&id).map(|state| state.mode) else {
        return;
    };
    if !matches!(
        mode,
        TcpMode::Listening { .. } | TcpMode::Closed { .. } | TcpMode::Connecting { .. }
    ) {
        let state = network.tcp_endpoints.get_mut(&id).unwrap();
        state.endpoint = Weak::new();
        state.orphaned = true;
        for entry in &mut state.handles {
            if entry.transport.can_recv() {
                entry.transport.abort();
            } else {
                entry.transport.close_orphan(super::super::now());
            }
        }
        network.queue_tcp_resets(id, super::super::now());
        if network.tcp_endpoints[&id].orphan_finished() {
            network.retire_tcp_orphan(id);
        }
        crate::worker::request_poll(network.domain);
        return;
    }
    disconnect_locked(network, id);
    let state = network
        .tcp_endpoints
        .remove(&id)
        .expect("TCP endpoint disappeared during final close");
    if let Some(lease) = state.port_lease {
        network.tcp_ports.release(lease);
    }
}
