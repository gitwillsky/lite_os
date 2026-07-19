use alloc::{sync::Arc, vec::Vec};

use smoltcp::socket::tcp::{self, State};

use crate::{fallible_tree::FallibleMap, ipc::PipeEnd};

use super::*;

/// @description 把一个 established listener handle 转移给新 TCP Socket/OFD facade。
/// @param socket listener identity。
/// @param notify accepted endpoint 拥有的 notification Pipe。
/// @return 持有原 established smoltcp handle 的 accepted endpoint。
/// @errors 返回 `Again`、状态、地址或分配错误，且不会丢失已建立连接。
pub(in crate::socket::inet) fn accept(
    socket: &InetSocket,
    notify: (Arc<PipeEnd>, Arc<PipeEnd>),
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
    let mut network = stack()?.lock();
    let (position, endpoint, backlog, port_lease) = {
        let state = network
            .tcp_endpoints
            .get(&listener_id)
            .ok_or(SocketError::NotConnected)?;
        let TcpMode::Listening { endpoint, backlog } = state.mode else {
            return Err(SocketError::Invalid);
        };
        let position = state
            .handles
            .iter()
            .position(|handle| {
                matches!(
                    network.sockets.get::<tcp::Socket<'static>>(*handle).state(),
                    State::Established | State::CloseWait
                )
            })
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
    let established_handle = network.tcp_endpoints[&listener_id].handles[position];
    let local_address = network
        .sockets
        .get::<tcp::Socket<'static>>(established_handle)
        .local_endpoint()
        .map(|local| from_ip(local.addr))
        .expect("established TCP listener child lost local endpoint");
    // exact-address node 在 replacement 加入 SocketSet 前预留；之后的 commit/handle transfer 均不分配。
    let prepared_lease = network
        .tcp_ports
        .prepare_retain_for_address(port_lease, local_address)
        .map_err(port_error)?;
    let id = allocate_endpoint_id(&mut network)?;
    let replacement = add_socket(&mut network)?;
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
    let handle = network
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
            .push(replacement);
    } else {
        network.sockets.remove(replacement);
    }
    let peer_closed = matches!(
        network.sockets.get::<tcp::Socket<'static>>(handle).state(),
        State::CloseWait
    );
    let options = network.tcp_endpoints[&listener_id].options;
    network
        .sockets
        .get_mut::<tcp::Socket<'static>>(handle)
        .set_nagle_enabled(!options.no_delay);
    accepted_handles.push(handle);
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            endpoint: Weak::new(),
            handles: accepted_handles,
            mode: TcpMode::Connected {
                peer_closed,
                shutdown_read: false,
            },
            pending_error: None,
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
            endpoint: InetEndpoint::Tcp(id),
            operation: spin::Mutex::new(()),
            notify_read: notify.0,
            notify_write: notify.1,
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
