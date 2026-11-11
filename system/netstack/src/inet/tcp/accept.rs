use alloc::{sync::Arc, vec::Vec};

use native_runtime::Event;

use crate::namespace::Namespace;
use fallible_tree::FallibleMap;

use super::*;

/// 把一个 established listener handle 转移给新 TCP endpoint。
///
/// # Parameters
///
/// - `socket`: listener identity。
/// - `readiness`: accepted endpoint 独占的就绪 Event。
/// - `namespace`: listener 所在的域。
///
/// # Returns
///
/// 持有原 established 连接的 accepted endpoint。
///
/// # Errors
///
/// 返回 `Again`、状态、地址或分配错误，且不会丢失已建立连接。
pub(in crate::inet) fn accept(
    socket: &InetSocket,
    readiness: Arc<Event>,
    namespace: Arc<Namespace>,
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
    let policy = network.tcp_endpoints[&listener_id].handles[position]
        .passive
        .as_ref()
        .expect("pending TCP child lost connection");
    // parent 改设备后 child 已独立持有原 membership；只有仍借 parent 的 child 才需要准备新节点。
    let prepared_lease = if policy.port_lease.is_none() {
        Some(
            network
                .tcp_ports
                .prepare_retain_for_address(
                    port_lease,
                    PortBinding::new(policy.connection.local.ip(), policy.options.ipv6_only)
                        .map_err(port_error)?
                        .with_interface(policy.options.bound_to_device),
                )
                .map_err(port_error)?,
        )
    } else {
        None
    };
    let id = allocate_endpoint_id(&mut network)?;
    if network.socket_count() + network.udp_endpoints.len() >= super::super::SOCKET_STORAGE_CAPACITY
    {
        return Err(SocketError::NoMemory);
    }
    let mut replacement = allocate_socket()?;
    let listener_options = network.tcp_endpoints[&listener_id].options;
    listener_options.apply_tcp(&mut replacement);
    replacement
        .listen(listen_endpoint(endpoint))
        .map_err(|_| SocketError::AddressNotAvailable)?;
    let accepted_lease = prepared_lease.map(|prepared| network.tcp_ports.commit_retained(prepared));
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
    }
    let policy = entry
        .passive
        .take()
        .expect("accepted child must have handshake policy");
    let options = policy.options;
    let accepted_lease = network.tcp_ports.set_reuse(
        policy
            .port_lease
            .or(accepted_lease)
            .expect("accepted child lost port lease"),
        options.reuse_address,
    );
    accepted_handles.push(entry);
    let family = network.tcp_endpoints[&listener_id].family;
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            family,
            endpoint: Weak::new(),
            handles: accepted_handles,
            mode: TcpMode::Connected(policy.connection),
            pending_error: policy.pending_error,
            port_lease: Some(accepted_lease),
            orphaned: false,
            options,
            md5_keys: policy.md5_keys,
            readiness: network_protocol::SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Arc::get_mut(&mut accepted_slot)
        .expect("new accepted endpoint Arc must be uniquely owned")
        .write(InetSocket {
            namespace,
            endpoint: InetEndpoint::Tcp(id),
            operation: crate::sync::Mutex::new(Some(InetEndpoint::Tcp(id))),
            readiness,
        });
    // SAFETY: accepted_slot 尚未克隆，且上一行已完整初始化 InetSocket storage。
    let accepted = unsafe { accepted_slot.assume_init() };
    network
        .tcp_endpoints
        .get_mut(&id)
        .expect("accepted TCP endpoint disappeared before Arc publication")
        .endpoint = Arc::downgrade(&accepted);
    drop(network);
    Ok(accepted)
}
