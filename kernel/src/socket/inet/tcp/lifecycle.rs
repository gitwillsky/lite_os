use super::*;

/// 新connect/listen复用同一个实际slot；reset已移交唯一有预算报文队列。
/// buffer准备可失败，OOM保留原bind与slot，报文不借用端口也不新增orphan身份。
pub(super) fn prepare_closed_transport(
    network: &mut NetworkStack,
    id: usize,
) -> Result<(), SocketError> {
    assert!(matches!(
        network.tcp_endpoints[&id].mode,
        TcpMode::Closed { .. }
    ));
    let send_mss = network
        .sockets
        .get::<tcp::Socket<'static>>(network.tcp_endpoints[&id].handles[0].handle)
        .send_mss();
    network.queue_tcp_resets(super::super::now());
    let NetworkStack {
        tcp_endpoints,
        sockets,
        ..
    } = network;
    let state = tcp_endpoints
        .get_mut(&id)
        .expect("closed endpoint disappeared");
    for entry in state.handles.drain(1..) {
        sockets.remove(entry.handle);
    }
    let handle = network.tcp_endpoints[&id].handles[0].handle;
    if network
        .sockets
        .get::<tcp::Socket<'static>>(handle)
        .recv_capacity()
        == 0
    {
        // 取消 SYN_SENT 后原 slot 保留零 buffer 的 CLOSED socket，维持 cleanup 容量证明。
        // 新 buffer 完整准备后才替换同一个 slot；OOM 不改变原 bind 或生命周期。
        let mut prepared = super::storage::allocate_socket()?;
        prepared.set_send_mss(send_mss);
        *network.sockets.get_mut::<tcp::Socket<'static>>(handle) = prepared;
    }
    let options = network.tcp_endpoints[&id].options;
    options.apply_tcp(network.sockets.get_mut::<tcp::Socket<'static>>(handle));
    Ok(())
}

/// native tcp_disconnect：清空实际连接/accept 队列，恢复原 bind；不等待 NIC 或分配。
pub(super) fn disconnect_locked(network: &mut NetworkStack, id: usize) {
    let NetworkStack {
        tcp_endpoints,
        sockets,
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
        _ => sockets
            .get::<tcp::Socket<'static>>(state.handles[0].handle)
            .send_mss(),
    };
    // native tcp_disconnect 清空 rx_opt（含 user_mss），但保留 mss_cache。
    state.options.user_mss = None;
    if !listener && let Some(entry) = state.handles.first() {
        let tcp = sockets.get::<tcp::Socket<'static>>(entry.handle);
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
        let tcp = sockets.get_mut::<tcp::Socket<'static>>(entry.handle);
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
        entry.passive = None;
        tcp.set_user_mss(None);
        tcp.set_send_mss(send_mss);
        if needs_reset {
            tcp.abort();
            retained = true;
            true
        } else if !retained {
            // 每个实际 facade 始终持至少一个 SocketSet slot；否则大量取消握手的
            // socket 会绕过 fixed cleanup ring 与 SocketSet capacity 的一对一界限。
            *tcp = placeholder_socket();
            tcp.set_send_mss(send_mss);
            retained = true;
            true
        } else {
            // native SYN_SENT/TIME_WAIT 等状态取消不发送 RST；直接释放实际 transport。
            sockets.remove(entry.handle);
            false
        }
    });
    if let Some(mut lease) = state.port_lease {
        if listener {
            lease = tcp_ports.stop_listener(lease);
        }
        state.port_lease = Some(
            tcp_ports.restore_bound_address(lease, bound.and_then(|bound| bound.addr).map(from_ip)),
        );
    }
    state.mode = TcpMode::Closed {
        bound,
        shutdown_read: false,
    };
    network.queue_tcp_resets(super::super::now());
}

/// AF_UNSPEC 主动取消 TCP 连接或监听；本地 bind identity 继续由同一 endpoint 持有。
pub(in crate::socket::inet) fn disconnect(socket: &InetSocket) -> Result<(), SocketError> {
    let mut network = socket.stack()?.lock()?;
    let id = endpoint_id(socket);
    if !network.tcp_endpoints.contains_key(&id) {
        return Err(SocketError::NotConnected);
    }
    disconnect_locked(&mut network, id);
    drop(network);
    crate::drivers::network::request_poll();
    Ok(())
}

/// 释放 TCP endpoint，同时保留 connected FIN/TIME_WAIT 协议生命周期。
///
/// # Parameters
///
/// - `network`: 已由 protocol owner 独占的完整 NetworkStack。
/// - `id`: 正在析构的 facade 所持稳定 endpoint id。
///
/// # Errors
///
/// endpoint 缺失或已删除时幂等忽略。
pub(in crate::socket::inet) fn drop_endpoint(network: &mut NetworkStack, id: usize) {
    let Some(mode) = network.tcp_endpoints.get(&id).map(|state| state.mode) else {
        return;
    };
    if !matches!(
        mode,
        TcpMode::Listening { .. } | TcpMode::Closed { .. } | TcpMode::Connecting { .. }
    ) {
        let NetworkStack {
            tcp_endpoints,
            sockets,
            ..
        } = &mut *network;
        let state = tcp_endpoints
            .get_mut(&id)
            .expect("TCP endpoint disappeared while stack lock is held");
        state.endpoint = Weak::new();
        state.orphaned = true;
        for entry in &state.handles {
            let handle = entry.handle;
            let tcp = sockets.get_mut::<tcp::Socket<'static>>(handle);
            // 最后 OFD 丢弃未读数据是 native abortive close，不能向 peer 宣称完整 FIN。
            if tcp.can_recv() {
                tcp.abort();
            } else {
                tcp.close_orphan(super::super::now());
            }
        }
        network.queue_tcp_resets(super::super::now());
        super::io::reap_orphans(network);
        crate::drivers::network::request_poll();
        return;
    }
    disconnect_locked(network, id);
    let state = network
        .tcp_endpoints
        .remove(&id)
        .expect("TCP endpoint disappeared while stack lock is held");
    if let Some(lease) = state.port_lease {
        network.tcp_ports.release(lease);
    }
    let handles = state.handles;
    for entry in handles {
        network.sockets.remove(entry.handle);
    }
}
