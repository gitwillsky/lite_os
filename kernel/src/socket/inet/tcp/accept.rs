use alloc::{sync::Arc, vec::Vec};

use smoltcp::socket::tcp;

use crate::{fallible_tree::FallibleMap, ipc::PipeEnd};

use super::*;

/// 把一个 established listener handle 转移给新 TCP Socket/OFD facade。
///
/// # Parameters
///
/// - `socket`: listener identity。
/// - `notify`: accepted endpoint 拥有的 notification Pipe。
/// - `timeouts`: 在取连接前准备的 policy allocation；成功后安装该 child 的握手快照。
///
/// # Returns
///
/// 持有原 established smoltcp handle 的 accepted endpoint。
///
/// # Errors
///
/// 返回 `Again`、状态、地址或分配错误，且不会丢失已建立连接。
pub(in crate::socket::inet) fn accept(
    socket: &InetSocket,
    notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    timeouts: Arc<SocketTimeouts>,
    memory: Arc<crate::socket::SocketMemory>,
) -> Result<Arc<InetSocket>, SocketError> {
    let mut accepted_slot =
        Arc::<InetSocket>::try_new_uninit().map_err(|_| SocketError::NoMemory)?;
    let mut accepted_handles = Vec::new();
    accepted_handles
        .try_reserve_exact(1)
        .map_err(|_| SocketError::NoMemory)?;
    let endpoint_slot = FallibleMap::<usize, TcpEndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    let listener_id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let (position, endpoint, backlog, port_lease) = {
        let state = network
            .tcp_endpoints
            .get(&listener_id)
            .ok_or(SocketError::NotConnected)?;
        let TcpMode::Listening {
            endpoint, backlog, ..
        } = state.mode
        else {
            return Err(SocketError::Invalid);
        };
        let position = state
            .handles
            .iter()
            .position(|handle| handle.passive.is_some())
            .ok_or(SocketError::Again)?;
        (
            position,
            endpoint,
            backlog,
            state
                .port_lease
                .expect("TCP listener lost local port lease"),
        )
    };
    let local_address = network.tcp_endpoints[&listener_id].handles[position]
        .passive
        .as_ref()
        .expect("pending TCP child lost connection")
        .connection
        .local
        .address;
    // exact-address node 在 replacement 加入 SocketSet 前预留；之后的 commit/handle transfer 均不分配。
    let prepared_lease = network
        .tcp_ports
        .prepare_retain_for_address(port_lease, local_address)
        .map_err(port_error)?;
    let id = allocate_endpoint_id(&mut network)?;
    let replacement = add_socket(&mut network)?;
    let listener_options = network.tcp_endpoints[&listener_id].options;
    listener_options.apply_tcp(network.sockets.get_mut::<tcp::Socket<'static>>(replacement));
    if network
        .sockets
        .get_mut::<tcp::Socket<'static>>(replacement)
        .listen(endpoint)
        .is_err()
    {
        network.sockets.remove(replacement);
        return Err(SocketError::AddressNotAvailable);
    }
    let accepted_lease = network.tcp_ports.commit_retained(prepared_lease);
    let mut entry = network
        .tcp_endpoints
        .get_mut(&listener_id)
        .expect("TCP listener disappeared while stack lock is held")
        .handles
        .remove(position);
    if network.tcp_endpoints[&listener_id].handles.len() < backlog {
        network
            .tcp_endpoints
            .get_mut(&listener_id)
            .expect("TCP listener disappeared while replenishing backlog")
            .handles
            .push(TcpHandle::new(replacement));
    } else {
        network.sockets.remove(replacement);
    }
    let policy = entry
        .passive
        .take()
        .expect("accepted child must have handshake policy");
    let options = policy.options;
    timeouts.initialize(policy.timeouts);
    let accepted_lease = network
        .tcp_ports
        .set_reuse(accepted_lease, options.reuse_address);
    accepted_handles.push(entry);
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            endpoint: Weak::new(),
            handles: accepted_handles,
            timeouts: timeouts.clone(),
            mode: TcpMode::Connected(policy.connection),
            pending_error: policy.pending_error,
            port_lease: Some(accepted_lease),
            orphaned: false,
            options,
            readiness: crate::socket::SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Arc::get_mut(&mut accepted_slot)
        .expect("new accepted endpoint Arc must be uniquely owned")
        .write(InetSocket {
            memory,
            endpoint: InetEndpoint::Tcp(id),
            operation: spin::Mutex::new(Some(InetEndpoint::Tcp(id))),
            notify_read: notify.0,
            notify_write: notify.1,
            timeouts,
        });
    // SAFETY: accepted_slot 尚未克隆，且上一行已完整初始化 InetSocket storage。
    let accepted = unsafe { accepted_slot.assume_init() };
    network
        .tcp_endpoints
        .get_mut(&id)
        .expect("accepted TCP endpoint disappeared before Arc publication")
        .endpoint = Arc::downgrade(&accepted);
    drop(network);
    socket.consume_notify();
    Ok(accepted)
}
