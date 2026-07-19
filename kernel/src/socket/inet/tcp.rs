use alloc::{sync::Weak, vec::Vec};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::tcp::{self, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::fallible_tree::FallibleMap;

use super::{
    InetEndpoint, InetSocket, InetSocketOptions, NetworkStack, PortLease, SocketError, from_ip,
    ipv4, port_error, stack,
};
use crate::socket::InetAddress;

#[path = "tcp/accept.rs"]
mod accept;
#[path = "tcp/io.rs"]
mod io;
#[path = "tcp/lifecycle.rs"]
mod lifecycle;
#[path = "tcp/storage.rs"]
mod storage;
pub(super) use accept::accept;
pub(super) use io::{maintain, poll_state, reap_orphans, receive, send, shutdown, take_error};
pub(super) use lifecycle::drop_endpoint;
use storage::{add_socket, placeholder_socket};

const TCP_BACKLOG_MAX: usize = 16;

#[derive(Clone, Copy)]
enum TcpMode {
    Fresh {
        bound: Option<IpListenEndpoint>,
    },
    Connecting,
    Connected {
        peer_closed: bool,
        shutdown_read: bool,
    },
    Listening {
        endpoint: IpListenEndpoint,
        backlog: usize,
    },
}

/// @description NetworkStack 唯一拥有的 TCP endpoint lifecycle 与 smoltcp handle 集合。
pub(super) struct TcpEndpointState {
    /// 只在释放 stack lock 后用于 readiness notification 的 OFD-facing 弱引用。
    pub(super) endpoint: Weak<InetSocket>,
    handles: Vec<SocketHandle>,
    mode: TcpMode,
    pending_error: Option<SocketError>,
    pub(super) port_lease: Option<PortLease>,
    // 最后一个 facade 释放后保留 endpoint state，直到 TCP close 完成；若改用独立 orphan
    // queue，析构路径会因 queue 扩容而出现无法返回 ENOMEM 的失败点。
    orphaned: bool,
    /// listener accept 继承同一个 SOL_SOCKET policy；缺失会让 accepted socket 丢失 device binding。
    pub(super) options: InetSocketOptions,
    // 协议 poll 前的唯一 edge 快照；缺失时长期 writable TCP 会持续唤醒全部 waiter。
    pub(super) readiness: crate::socket::SocketPollState,
    // 只跨越 stack unlock 保存一次 transition；缺失会在持 stack lock 时反向进入 wait owner。
    pub(super) notification_pending: bool,
}

fn allocate_endpoint_id(network: &mut NetworkStack) -> Result<usize, SocketError> {
    let id = network.next_tcp_id;
    network.next_tcp_id = network
        .next_tcp_id
        .checked_add(1)
        .ok_or(SocketError::NoMemory)?;
    Ok(id)
}

/// @description 分配 closed TCP handle，并注册唯一 endpoint state。
/// @param network 唯一 NetworkStack owner。
/// @param endpoint facade weak identity；调用方返回后立即发布完整 Arc。
/// @return 稳定 TCP endpoint id。
/// @errors id 或 buffer 分配失败返回 `NoMemory`。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
) -> Result<usize, SocketError> {
    let endpoint_slot = FallibleMap::<usize, TcpEndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| SocketError::NoMemory)?;
    let id = allocate_endpoint_id(network)?;
    let handle = add_socket(network)?;
    handles.push(handle);
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            endpoint,
            handles,
            mode: TcpMode::Fresh { bound: None },
            pending_error: None,
            port_lease: None,
            orphaned: false,
            options: InetSocketOptions::default(),
            readiness: crate::socket::SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Ok(id)
}

pub(super) fn set_no_delay(socket: &InetSocket, enabled: bool) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock()?;
    let NetworkStack {
        tcp_endpoints,
        sockets,
        ..
    } = &mut *network;
    let state = tcp_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    for &handle in &state.handles {
        sockets
            .get_mut::<tcp::Socket<'static>>(handle)
            .set_nagle_enabled(!enabled);
    }
    state.options.no_delay = enabled;
    Ok(())
}

fn endpoint_id(socket: &InetSocket) -> usize {
    match socket.endpoint {
        InetEndpoint::Tcp(id) => id,
        InetEndpoint::Udp(_) | InetEndpoint::Raw(_) => {
            unreachable!("TCP operation reached non-TCP endpoint")
        }
    }
}

fn listen_endpoint(address: InetAddress) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: (!address.address.is_unspecified()).then(|| ipv4(address.address)),
        port: address.port,
    }
}

/// @description 绑定 fresh TCP endpoint；port 0 经唯一 allocator 分配 ephemeral port。
/// @param socket TCP facade identity。
/// @param address 请求的 IPv4 address 与 port。
/// @return 成功取得本地 endpoint 后返回 unit。
/// @errors 返回地址、状态、冲突或分配错误。
pub(super) fn bind(socket: &InetSocket, address: InetAddress) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock()?;
    let address_filter = (!address.address.is_unspecified()).then_some(address.address);
    if address_filter.is_some_and(|candidate| {
        network.interface_state.address != Some(candidate) || !network.interface_state.up
    }) {
        return Err(SocketError::AddressNotAvailable);
    }
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !matches!(state.mode, TcpMode::Fresh { bound: None }) {
        return Err(SocketError::Invalid);
    }
    let reuse_address = state.options.reuse_address;
    let lease = if address.port == 0 {
        network
            .tcp_ports
            .acquire_ephemeral(address_filter, reuse_address)
            .map_err(port_error)?
    } else {
        network
            .tcp_ports
            .acquire(address.port, address_filter, reuse_address)
            .map_err(port_error)?
    };
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP endpoint disappeared while stack lock is held");
    state.mode = TcpMode::Fresh {
        bound: Some(listen_endpoint(InetAddress {
            port: lease.port(),
            ..address
        })),
    };
    state.port_lease = Some(lease);
    Ok(())
}

/// @description 将 fresh TCP endpoint 原子转换为有界 passive listener。
/// @param socket TCP facade identity。
/// @param backlog 请求深度，截断到文档声明的 kernel 上限。
/// @return 全部 listen handle 就绪后返回 unit。
/// @errors 返回状态、地址或分配错误，且不会发布半初始化 listener。
pub(super) fn listen(socket: &InetSocket, backlog: usize) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let (bound, reuse_address) = match state.mode {
        TcpMode::Fresh { bound } => (bound, state.options.reuse_address),
        TcpMode::Listening { .. } => return Ok(()),
        _ => return Err(SocketError::Invalid),
    };
    let backlog = backlog.clamp(1, TCP_BACKLOG_MAX);
    network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP endpoint disappeared while stack lock is held")
        .handles
        .try_reserve_exact(backlog.saturating_sub(1))
        .map_err(|_| SocketError::NoMemory)?;
    let mut extra = Vec::new();
    extra
        .try_reserve_exact(backlog.saturating_sub(1))
        .map_err(|_| SocketError::NoMemory)?;
    for _ in 1..backlog {
        match add_socket(&mut network) {
            Ok(handle) => extra.push(handle),
            Err(error) => {
                for handle in extra {
                    network.sockets.remove(handle);
                }
                return Err(error);
            }
        }
    }
    let new_lease = if bound.is_none() {
        match network.tcp_ports.acquire_ephemeral(None, reuse_address) {
            Ok(lease) => Some(lease),
            Err(error) => {
                for handle in extra {
                    network.sockets.remove(handle);
                }
                return Err(port_error(error));
            }
        }
    } else {
        None
    };
    let endpoint = match (bound, new_lease) {
        (Some(endpoint), _) => endpoint,
        (None, Some(lease)) => IpListenEndpoint {
            addr: None,
            port: lease.port(),
        },
        (None, None) => return Err(SocketError::AddressNotAvailable),
    };
    let base_lease = new_lease.unwrap_or_else(|| {
        network.tcp_endpoints[&id]
            .port_lease
            .expect("bound TCP endpoint lost local port lease")
    });
    let listener_lease = match network.tcp_ports.claim_listener(base_lease) {
        Ok(lease) => lease,
        Err(error) => {
            for handle in extra {
                network.sockets.remove(handle);
            }
            if let Some(lease) = new_lease {
                network.tcp_ports.release(lease);
            }
            return Err(port_error(error));
        }
    };
    let first = network.tcp_endpoints[&id].handles[0];
    for handle in core::iter::once(first).chain(extra.iter().copied()) {
        if network
            .sockets
            .get_mut::<tcp::Socket<'static>>(handle)
            .listen(endpoint)
            .is_err()
        {
            network
                .sockets
                .get_mut::<tcp::Socket<'static>>(first)
                .abort();
            for handle in extra {
                network.sockets.remove(handle);
            }
            network.tcp_ports.release_listener_claim(listener_lease);
            if let Some(lease) = new_lease {
                network.tcp_ports.release(lease);
            }
            return Err(SocketError::AddressNotAvailable);
        }
    }
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP listener endpoint disappeared during atomic publication");
    state.handles.extend(extra);
    state.mode = TcpMode::Listening { endpoint, backlog };
    state.port_lease = Some(listener_lease);
    Ok(())
}

/// @description 通过唯一 interface context 启动 active TCP handshake。
/// @param socket TCP facade identity。
/// @param peer 远端 IPv4 endpoint。
/// @return SYN 提交后返回 `InProgress`；完成状态通过 readiness 观察。
/// @errors 返回标准地址、route、状态或 in-progress 错误。
pub(super) fn connect(socket: &InetSocket, peer: InetAddress) -> Result<(), SocketError> {
    if peer.port == 0 || peer.address.is_unspecified() {
        return Err(SocketError::AddressNotAvailable);
    }
    let id = endpoint_id(socket);
    let mut network = stack()?.lock()?;
    if !network.interface_state.up || network.interface_state.address.is_none() {
        return Err(SocketError::NetworkUnreachable);
    }
    let (bound, reuse_address, existing_lease) = match network.tcp_endpoints.get(&id) {
        Some(state) => match state.mode {
            TcpMode::Fresh { bound } => (bound, state.options.reuse_address, state.port_lease),
            TcpMode::Connecting => return Err(SocketError::AlreadyInProgress),
            TcpMode::Connected { .. } => return Err(SocketError::AlreadyConnected),
            TcpMode::Listening { .. } => return Err(SocketError::Invalid),
        },
        None => return Err(SocketError::NotConnected),
    };
    let local_address = bound
        .and_then(|endpoint| endpoint.addr)
        .map(from_ip)
        .or(network.interface_state.address)
        .expect("up TCP interface lost source address");
    let new_lease = if bound.is_none() {
        Some(
            network
                .tcp_ports
                .acquire_ephemeral(Some(local_address), reuse_address)
                .map_err(port_error)?,
        )
    } else {
        None
    };
    let prepared_readdress = existing_lease
        .map(|lease| network.tcp_ports.prepare_readdress(lease, local_address))
        .transpose()
        .map_err(port_error)?;
    let local = match (bound, new_lease) {
        (Some(endpoint), _) => endpoint,
        (None, Some(lease)) => IpListenEndpoint {
            addr: None,
            port: lease.port(),
        },
        (None, None) => return Err(SocketError::AddressNotAvailable),
    };
    let handle = network.tcp_endpoints[&id].handles[0];
    let NetworkStack {
        interface, sockets, ..
    } = &mut *network;
    let connect = sockets.get_mut::<tcp::Socket<'static>>(handle).connect(
        interface.context(),
        IpEndpoint::new(ipv4(peer.address), peer.port),
        local,
    );
    if connect.is_err() {
        if let Some(lease) = new_lease {
            network.tcp_ports.release(lease);
        }
        return Err(SocketError::AddressNotAvailable);
    }
    debug_assert_eq!(
        network
            .sockets
            .get::<tcp::Socket<'static>>(handle)
            .local_endpoint()
            .map(|endpoint| from_ip(endpoint.addr)),
        Some(local_address)
    );
    let connected_lease = if let Some(lease) = new_lease {
        lease
    } else {
        network.tcp_ports.commit_readdress(
            prepared_readdress.expect("bound TCP connect lost prepared port readdress"),
        )
    };
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP endpoint disappeared during connect publication");
    state.mode = TcpMode::Connecting;
    state.port_lease = Some(connected_lease);
    drop(network);
    crate::drivers::network::request_poll();
    Err(SocketError::InProgress)
}

/// @description 读取权威 local TCP endpoint。
/// @param socket TCP facade identity。
/// @return bound/listening/connected 或 unspecified 本地地址。
/// @errors endpoint 删除后返回 `NotConnected`。
pub(super) fn address(socket: &InetSocket) -> Result<InetAddress, SocketError> {
    let id = endpoint_id(socket);
    let network = stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let endpoint = match state.mode {
        TcpMode::Fresh { bound } => bound.map(|endpoint| IpEndpoint {
            addr: endpoint.addr.unwrap_or_else(|| ipv4(Ipv4Addr::UNSPECIFIED)),
            port: endpoint.port,
        }),
        TcpMode::Listening { endpoint, .. } => Some(IpEndpoint {
            addr: endpoint.addr.unwrap_or_else(|| ipv4(Ipv4Addr::UNSPECIFIED)),
            port: endpoint.port,
        }),
        TcpMode::Connecting | TcpMode::Connected { .. } => network
            .sockets
            .get::<tcp::Socket<'static>>(state.handles[0])
            .local_endpoint(),
    };
    Ok(endpoint.map_or(
        InetAddress {
            address: Ipv4Addr::UNSPECIFIED,
            port: 0,
        },
        |endpoint| InetAddress {
            address: from_ip(endpoint.addr),
            port: endpoint.port,
        },
    ))
}

/// @description 读取权威 connected TCP peer endpoint。
/// @param socket TCP facade identity。
/// @return 远端 IPv4 endpoint。
/// @errors tuple 尚未建立或 endpoint 已删除时返回 `NotConnected`。
pub(super) fn peer_address(socket: &InetSocket) -> Result<InetAddress, SocketError> {
    let id = endpoint_id(socket);
    let network = stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    network
        .sockets
        .get::<tcp::Socket<'static>>(state.handles[0])
        .remote_endpoint()
        .map(|endpoint| InetAddress {
            address: from_ip(endpoint.addr),
            port: endpoint.port,
        })
        .ok_or(SocketError::NotConnected)
}

/// @description 在 OFD writable/error wakeup 后解析 active-connect 完成状态。
/// @param socket TCP facade identity。
/// @return 仅在 established 后返回 unit。
/// @errors 返回 in-progress、refusal 或无效状态错误。
pub(super) fn connection_result(socket: &InetSocket) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let network = stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    match state.mode {
        TcpMode::Connected { .. } => Ok(()),
        TcpMode::Connecting => match network
            .sockets
            .get::<tcp::Socket<'static>>(state.handles[0])
            .state()
        {
            State::Established => Ok(()),
            State::Closed => Err(state
                .pending_error
                .unwrap_or(SocketError::ConnectionRefused)),
            _ => Err(SocketError::InProgress),
        },
        _ => Err(SocketError::NotConnected),
    }
}
