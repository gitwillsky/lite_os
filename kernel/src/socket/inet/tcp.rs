use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::tcp::{self, CongestionControl, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::ipc::PipeEnd;

use super::{
    EPHEMERAL_END, EPHEMERAL_START, InetEndpoint, InetSocket, NetworkStack, SocketError, from_ip,
    ipv4, now, stack,
};
use crate::socket::InetAddress;

#[path = "tcp/io.rs"]
mod io;
pub(super) use io::{maintain, poll_state, receive, send, shutdown, take_error};

const TCP_BUFFER_BYTES: usize = 32 * 1024;
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
}

fn allocate_buffer() -> Result<Vec<u8>, SocketError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(TCP_BUFFER_BYTES)
        .map_err(|_| SocketError::NoMemory)?;
    bytes.resize(TCP_BUFFER_BYTES, 0);
    Ok(bytes)
}

fn add_socket(network: &mut NetworkStack) -> Result<SocketHandle, SocketError> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(allocate_buffer()?),
        tcp::SocketBuffer::new(allocate_buffer()?),
    );
    // Reno 不使用 kernel FPU context，且比关闭 congestion control 更符合共享网络语义。
    socket.set_congestion_control(CongestionControl::Reno);
    Ok(network.sockets.add(socket))
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
    let id = allocate_endpoint_id(network)?;
    let handle = add_socket(network)?;
    network.tcp_endpoints.insert(
        id,
        TcpEndpointState {
            endpoint,
            handles: alloc::vec![handle],
            mode: TcpMode::Fresh { bound: None },
            pending_error: None,
        },
    );
    Ok(id)
}

fn endpoint_id(socket: &InetSocket) -> usize {
    match socket.endpoint {
        InetEndpoint::Tcp(id) => id,
        InetEndpoint::Udp(_) => unreachable!("TCP operation reached UDP endpoint"),
    }
}

fn listen_endpoint(address: InetAddress) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: (!address.address.is_unspecified()).then(|| ipv4(address.address)),
        port: address.port,
    }
}

impl NetworkStack {
    fn tcp_port_in_use(&self, address: Option<Ipv4Addr>, port: u16, except: usize) -> bool {
        self.tcp_endpoints.iter().any(|(&id, state)| {
            if id == except {
                return false;
            }
            let local = match state.mode {
                TcpMode::Fresh { bound } => bound,
                TcpMode::Listening { endpoint, .. } => Some(endpoint),
                TcpMode::Connecting | TcpMode::Connected { .. } => state
                    .handles
                    .first()
                    .and_then(|handle| {
                        self.sockets
                            .get::<tcp::Socket<'static>>(*handle)
                            .local_endpoint()
                    })
                    .map(|endpoint| IpListenEndpoint {
                        addr: Some(endpoint.addr),
                        port: endpoint.port,
                    }),
            };
            local.is_some_and(|local| {
                local.port == port
                    && (local.addr.is_none()
                        || address.is_none()
                        || local.addr == address.map(ipv4))
            })
        })
    }

    fn allocate_tcp_ephemeral(&mut self, id: usize) -> Result<u16, SocketError> {
        for _ in EPHEMERAL_START..=EPHEMERAL_END {
            let candidate = self.next_tcp_ephemeral;
            self.next_tcp_ephemeral = if candidate == EPHEMERAL_END {
                EPHEMERAL_START
            } else {
                candidate + 1
            };
            if !self.tcp_port_in_use(None, candidate, id) {
                return Ok(candidate);
            }
        }
        Err(SocketError::AddressInUse)
    }
}

/// @description 绑定 fresh TCP endpoint；port 0 经唯一 allocator 分配 ephemeral port。
/// @param socket TCP facade identity。
/// @param address 请求的 IPv4 address 与 port。
/// @return 成功取得本地 endpoint 后返回 unit。
/// @errors 返回地址、状态、冲突或分配错误。
pub(super) fn bind(socket: &InetSocket, address: InetAddress) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock();
    let address_filter = (!address.address.is_unspecified()).then_some(address.address);
    if address_filter.is_some_and(|candidate| {
        network.interface_state.address != Some(candidate) || !network.interface_state.up
    }) {
        return Err(SocketError::AddressNotAvailable);
    }
    let port = if address.port == 0 {
        network.allocate_tcp_ephemeral(id)?
    } else {
        address.port
    };
    if network.tcp_port_in_use(address_filter, port, id) {
        return Err(SocketError::AddressInUse);
    }
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    match state.mode {
        TcpMode::Fresh { bound: None } => {
            state.mode = TcpMode::Fresh {
                bound: Some(listen_endpoint(InetAddress { port, ..address })),
            };
            Ok(())
        }
        TcpMode::Fresh { bound: Some(_) } => Err(SocketError::Invalid),
        _ => Err(SocketError::Invalid),
    }
}

/// @description 将 fresh TCP endpoint 原子转换为有界 passive listener。
/// @param socket TCP facade identity。
/// @param backlog 请求深度，截断到文档声明的 kernel 上限。
/// @return 全部 listen handle 就绪后返回 unit。
/// @errors 返回状态、地址或分配错误，且不会发布半初始化 listener。
pub(super) fn listen(socket: &InetSocket, backlog: usize) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock();
    let bound = match network.tcp_endpoints.get(&id).map(|state| state.mode) {
        Some(TcpMode::Fresh { bound }) => bound,
        Some(TcpMode::Listening { .. }) => return Ok(()),
        Some(_) => return Err(SocketError::Invalid),
        None => return Err(SocketError::NotConnected),
    };
    let endpoint = match bound {
        Some(endpoint) => endpoint,
        None => IpListenEndpoint {
            addr: None,
            port: network.allocate_tcp_ephemeral(id)?,
        },
    };
    let backlog = backlog.clamp(1, TCP_BACKLOG_MAX);
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
            return Err(SocketError::AddressNotAvailable);
        }
    }
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP listener endpoint disappeared during atomic publication");
    state.handles.extend(extra);
    state.mode = TcpMode::Listening { endpoint, backlog };
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
    let mut network = stack()?.lock();
    if !network.interface_state.up || network.interface_state.address.is_none() {
        return Err(SocketError::NetworkUnreachable);
    }
    let bound = match network.tcp_endpoints.get(&id).map(|state| state.mode) {
        Some(TcpMode::Fresh { bound }) => bound,
        Some(TcpMode::Connecting) => return Err(SocketError::AlreadyInProgress),
        Some(TcpMode::Connected { .. }) => return Err(SocketError::AlreadyConnected),
        Some(TcpMode::Listening { .. }) => return Err(SocketError::Invalid),
        None => return Err(SocketError::NotConnected),
    };
    let local = match bound {
        Some(endpoint) => endpoint,
        None => IpListenEndpoint {
            addr: None,
            port: network.allocate_tcp_ephemeral(id)?,
        },
    };
    let handle = network.tcp_endpoints[&id].handles[0];
    let NetworkStack {
        interface, sockets, ..
    } = &mut *network;
    sockets
        .get_mut::<tcp::Socket<'static>>(handle)
        .connect(
            interface.context(),
            IpEndpoint::new(ipv4(peer.address), peer.port),
            local,
        )
        .map_err(|_| SocketError::AddressNotAvailable)?;
    network
        .tcp_endpoints
        .get_mut(&id)
        .expect("TCP endpoint disappeared during connect publication")
        .mode = TcpMode::Connecting;
    let NetworkStack {
        interface,
        device,
        sockets,
        ..
    } = &mut *network;
    interface.poll_egress(now(), device, sockets);
    Err(SocketError::InProgress)
}

/// @description 把一个 established listener handle 转移给新 TCP Socket/OFD facade。
/// @param socket listener identity。
/// @param notify accepted endpoint 拥有的 notification Pipe。
/// @return 持有原 established smoltcp handle 的 accepted endpoint。
/// @errors 返回 `Again`、状态、地址或分配错误，且不会丢失已建立连接。
pub(super) fn accept(
    socket: &InetSocket,
    notify: (Arc<PipeEnd>, Arc<PipeEnd>),
) -> Result<Arc<InetSocket>, SocketError> {
    let listener_id = endpoint_id(socket);
    let mut network = stack()?.lock();
    let (position, endpoint, backlog) = {
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
        (position, endpoint, backlog)
    };
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
    let accepted = Arc::new(InetSocket {
        endpoint: InetEndpoint::Tcp(id),
        notify_read: notify.0,
        notify_write: notify.1,
    });
    let peer_closed = matches!(
        network.sockets.get::<tcp::Socket<'static>>(handle).state(),
        State::CloseWait
    );
    network.tcp_endpoints.insert(
        id,
        TcpEndpointState {
            endpoint: Arc::downgrade(&accepted),
            handles: alloc::vec![handle],
            mode: TcpMode::Connected {
                peer_closed,
                shutdown_read: false,
            },
            pending_error: None,
        },
    );
    drop(network);
    socket.consume_notify();
    Ok(accepted)
}

/// @description 读取权威 local TCP endpoint。
/// @param socket TCP facade identity。
/// @return bound/listening/connected 或 unspecified 本地地址。
/// @errors endpoint 删除后返回 `NotConnected`。
pub(super) fn address(socket: &InetSocket) -> Result<InetAddress, SocketError> {
    let id = endpoint_id(socket);
    let network = stack()?.lock();
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
    let network = stack()?.lock();
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
    let network = stack()?.lock();
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

/// @description 释放 TCP endpoint，同时保留 connected FIN/TIME_WAIT 协议生命周期。
/// @param id 正在析构的 facade 所持稳定 endpoint id。
/// @return 无返回值。
/// @errors endpoint 缺失或已删除时幂等忽略。
pub(super) fn drop_endpoint(id: usize) {
    let Some(stack) = super::NETWORK_STACK.get() else {
        return;
    };
    let mut network = stack.lock();
    let Some(state) = network.tcp_endpoints.remove(&id) else {
        return;
    };
    if matches!(
        state.mode,
        TcpMode::Listening { .. } | TcpMode::Fresh { .. } | TcpMode::Connecting
    ) {
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
    } else {
        for handle in state.handles {
            network
                .sockets
                .get_mut::<tcp::Socket<'static>>(handle)
                .close();
            network.orphaned_tcp.push(handle);
        }
        let NetworkStack {
            interface,
            device,
            sockets,
            ..
        } = &mut *network;
        interface.poll_egress(now(), device, sockets);
    }
}
