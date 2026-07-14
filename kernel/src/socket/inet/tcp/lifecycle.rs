use super::*;

/// 释放 TCP endpoint，同时保留 connected FIN/TIME_WAIT 协议生命周期。
///
/// @param id 正在析构的 facade 所持稳定 endpoint id。
/// @return 无返回值。
/// @errors endpoint 缺失或已删除时幂等忽略。
pub(in crate::socket::inet) fn drop_endpoint(id: usize) {
    let Ok(stack) = stack() else {
        return;
    };
    let mut network = stack.lock();
    let Some(mode) = network.tcp_endpoints.get(&id).map(|state| state.mode) else {
        return;
    };
    if !matches!(
        mode,
        TcpMode::Listening { .. } | TcpMode::Fresh { .. } | TcpMode::Connecting
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
        for &handle in &state.handles {
            sockets.get_mut::<tcp::Socket<'static>>(handle).close();
        }
        let NetworkStack {
            interface,
            device,
            sockets,
            ..
        } = &mut *network;
        interface.poll_egress(now(), device, sockets);
        return;
    }
    let state = network
        .tcp_endpoints
        .remove(&id)
        .expect("TCP endpoint disappeared while stack lock is held");
    let handles = state.handles;
    let needs_reset = handles.iter().any(|handle| {
        network
            .sockets
            .get::<tcp::Socket<'static>>(*handle)
            .remote_endpoint()
            .is_some()
    });
    for &handle in &handles {
        network
            .sockets
            .get_mut::<tcp::Socket<'static>>(handle)
            .abort();
    }
    if needs_reset {
        let NetworkStack {
            interface,
            device,
            sockets,
            ..
        } = &mut *network;
        interface.poll_egress(now(), device, sockets);
    }
    for handle in handles {
        network.sockets.remove(handle);
    }
}
