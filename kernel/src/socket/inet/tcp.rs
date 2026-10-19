use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::tcp::{self, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::fallible_tree::FallibleMap;

use super::{
    InetEndpoint, InetSocket, InetSocketOptions, NetworkStack, PortLease, SocketError, from_ip,
    ipv4, port_error,
};
use crate::socket::InetAddress;
use crate::socket::{SocketTimeouts, timeouts::TimeoutValues};

mod accept;
mod io;
mod lifecycle;
mod storage;
pub(super) use accept::accept;
pub(super) use io::{
    maintain, maintain_clocks, poll_state, reap_orphans, receive, send, shutdown, take_error,
    try_poll_state,
};
pub(super) use lifecycle::{disconnect, drop_endpoint};
use storage::{add_socket, placeholder_socket};

const TCP_BACKLOG_MAX: usize = 16;

#[derive(Clone, Copy)]
enum TcpMode {
    Closed {
        bound: Option<IpListenEndpoint>,
        shutdown_read: bool,
    },
    Connecting {
        local: InetAddress,
        bound: IpListenEndpoint,
        // SYN_RECV 半关闭仍属于未完成的实际握手；最终 ACK 才移交权限并发送 FIN。
        shutdown_read: bool,
        shutdown_write: bool,
        // 已交付一次实际 handshake failure；SO_ERROR 消费不重新生成错误。
        terminal: bool,
    },
    Connected(TcpConnection),
    Listening {
        endpoint: IpListenEndpoint,
        backlog: usize,
        // 实际 parent listener 的 mss_cache；每个 passive child 自己持协商 cache，不借 child 反推。
        send_mss: usize,
    },
}

/// 实际连接的地址与半关闭生命周期；协议库 tuple 仅负责路由，RST 清除它不释放绑定。
#[derive(Clone, Copy)]
struct TcpConnection {
    // 原始 bind identity，与握手选择的 source address 不同；disconnect 必须恢复 user bind。
    bound: IpListenEndpoint,
    local: InetAddress,
    peer: InetAddress,
    peer_closed: bool,
    shutdown_read: bool,
    // 实际 SEND_SHUTDOWN permission；SYN_RECV 关闭写侧须等最终 ACK 后才提交 FIN。
    shutdown_write: bool,
    // OWNER: 记录实际 Closed transition 是否已交付；缺失会在 SO_ERROR 消费后重填 RESET。
    terminal: bool,
}

impl TcpConnection {
    fn established(tcp: &tcp::Socket<'_>, bound: IpListenEndpoint) -> Self {
        let local = tcp
            .local_endpoint()
            .expect("established TCP lost local tuple");
        let peer = tcp
            .remote_endpoint()
            .expect("established TCP lost peer tuple");
        Self {
            bound,
            local: InetAddress {
                address: from_ip(local.addr),
                port: local.port,
            },
            peer: InetAddress {
                address: from_ip(peer.addr),
                port: peer.port,
            },
            peer_closed: tcp.state() == State::CloseWait,
            shutdown_read: false,
            shutdown_write: false,
            terminal: false,
        }
    }

    /// 消费一次实际终止事件；FIN 后的合法 RST 仍产生 SO_ERROR，但读 EOF 优先。
    fn observe(
        &mut self,
        tcp: &mut tcp::Socket<'_>,
        reset: Option<super::device::TcpReset>,
    ) -> Option<SocketError> {
        if self.shutdown_write && matches!(tcp.state(), State::Established | State::CloseWait) {
            tcp.close();
        }
        if matches!(
            tcp.state(),
            State::CloseWait | State::LastAck | State::Closing | State::TimeWait
        ) {
            self.peer_closed = true;
        }
        if tcp.state() != State::Closed || self.terminal {
            return None;
        }
        self.terminal = true;
        if tcp.failure_reason() == Some(tcp::FailureReason::TimedOut) {
            return Some(SocketError::TimedOut);
        }
        let reset = reset.is_some_and(|reset| reset.matches(self.local, self.peer));
        if reset || !self.peer_closed {
            Some(SocketError::ConnectionReset)
        } else {
            None
        }
    }
}

/// 实际 passive child 的不可变握手快照；accept 后由新 endpoint 唯一保存 live policy。
struct PassivePolicy {
    timeouts: TimeoutValues,
    options: InetSocketOptions,
    connection: TcpConnection,
    pending_error: Option<SocketError>,
}

/// 一个实际 smoltcp handle 及其尚未交付的 child policy；不另建平行 pending index。
struct TcpHandle {
    handle: SocketHandle,
    passive: Option<PassivePolicy>,
}
impl TcpHandle {
    fn new(handle: SocketHandle) -> Self {
        Self {
            handle,
            passive: None,
        }
    }
}

/// NetworkStack 唯一拥有的 TCP endpoint lifecycle 与 smoltcp handle 集合。
pub(super) struct TcpEndpointState {
    /// 只在释放 stack lock 后用于 readiness notification 的 OFD-facing 弱引用。
    pub(super) endpoint: Weak<InetSocket>,
    handles: Vec<TcpHandle>,
    // 与 InetSocket/OFD 共用同一 Mutex owner；pending child 只保留冻结的不可变值。
    timeouts: Arc<SocketTimeouts>,
    mode: TcpMode,
    pending_error: Option<SocketError>,
    pub(super) port_lease: Option<PortLease>,
    // 最后一个 facade 释放后保留 endpoint state，直到 TCP close 完成；若改用独立 orphan
    // queue，析构路径会因 queue 扩容而出现无法返回 ENOMEM 的失败点。
    orphaned: bool,
    /// 当前 endpoint 的 SOL_SOCKET policy；passive child 在握手完成时冻结，而非 accept 时复制。
    pub(super) options: InetSocketOptions,
    // 协议 poll 前的唯一 edge 快照；缺失时长期 writable TCP 会持续唤醒全部 waiter。
    pub(super) readiness: crate::socket::SocketPollState,
    // 只跨越 stack unlock 保存一次 transition；缺失会在持 stack lock 时反向进入 wait owner。
    pub(super) notification_pending: bool,
}

#[cfg(test)]
impl TcpEndpointState {
    /// host 回归只观察实际 handle，不扩大 production lifecycle 字段可见性。
    pub(super) fn first_handle_for_test(&self) -> SocketHandle {
        self.handles[0].handle
    }
    pub(super) fn pending_error_for_test(&self) -> Option<SocketError> {
        self.pending_error
    }
}

fn allocate_endpoint_id(network: &mut NetworkStack) -> Result<usize, SocketError> {
    let id = network.next_tcp_id;
    network.next_tcp_id = network
        .next_tcp_id
        .checked_add(1)
        .ok_or(SocketError::NoMemory)?;
    Ok(id)
}

/// 分配 closed TCP handle，并注册唯一 endpoint state。
///
/// # Parameters
///
/// - `network`: 唯一 NetworkStack owner。
/// - `endpoint`: facade weak identity；调用方返回后立即发布完整 Arc。
/// - `timeouts`: 与 facade 同一实际 owner，passive child 据此冻结握手快照。
///
/// # Returns
///
/// 稳定 TCP endpoint id。
///
/// # Errors
///
/// id 或 buffer 分配失败返回 `NoMemory`。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
    timeouts: Arc<SocketTimeouts>,
) -> Result<usize, SocketError> {
    let endpoint_slot = FallibleMap::<usize, TcpEndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| SocketError::NoMemory)?;
    let id = allocate_endpoint_id(network)?;
    let handle = add_socket(network)?;
    handles.push(TcpHandle::new(handle));
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            endpoint,
            handles,
            timeouts,
            mode: TcpMode::Closed {
                bound: None,
                shutdown_read: false,
            },
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
    update_transport_options(socket, |options| options.no_delay = enabled)
}

pub(super) fn set_timer_option(
    socket: &InetSocket,
    option: super::InternetOption,
    value: i32,
) -> Result<(), SocketError> {
    update_transport_options(socket, |options| match option {
        super::InternetOption::KeepAlive => options.keep_alive = value != 0,
        super::InternetOption::KeepIdle => options.keep_idle = value as u16,
        super::InternetOption::KeepInterval => options.keep_interval = value as u16,
        super::InternetOption::KeepCount => options.keep_count = value as u8,
        super::InternetOption::SynRetries => options.syn_retries = value as u8,
        super::InternetOption::UserTimeout => options.user_timeout = value as u32,
        super::InternetOption::Linger2 => options.linger2 = value.clamp(-1, 120),
        _ => unreachable!("only decoded native timer policy reaches TCP owner"),
    })
}

/// native IP_TTL 更新 active/listening handles，保留已建立 passive child 的冻结 policy。
pub(super) fn set_hop_limit(socket: &InetSocket, value: Option<u8>) -> Result<(), SocketError> {
    update_transport_options(socket, |options| options.hop_limit = value)
}

pub(super) fn set_max_segment(socket: &InetSocket, value: Option<u16>) -> Result<(), SocketError> {
    update_transport_options(socket, |options| options.user_mss = value)
}

/// 已在同一network guard提交native parent policy；只更新未冻结child的实际准入机制。
pub(super) fn apply_bound_interface(
    network: &mut NetworkStack,
    id: usize,
    index: Option<u32>,
) -> Result<(), SocketError> {
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    debug_assert_eq!(state.options.bound_to_device, index);
    for entry in &state.handles {
        if entry.passive.is_none() {
            network
                .sockets
                .get_mut::<tcp::Socket<'static>>(entry.handle)
                .set_bound_interface(index);
        }
    }
    Ok(())
}

/// 读取唯一实际 handle 的 configured/negotiated MSS；listening policy 不借 frozen child。
pub(super) fn max_segment_size(network: &NetworkStack, id: usize) -> Result<i32, SocketError> {
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if let TcpMode::Listening { send_mss, .. } = state.mode {
        return Ok(state.options.user_mss.map_or(send_mss as i32, i32::from));
    }
    let handle = state
        .handles
        .first()
        .ok_or(SocketError::NotConnected)?
        .handle;
    Ok(network
        .sockets
        .get::<tcp::Socket<'static>>(handle)
        .max_segment_size() as i32)
}

fn update_transport_options(
    socket: &InetSocket,
    update: impl FnOnce(&mut InetSocketOptions),
) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let NetworkStack {
        tcp_endpoints,
        sockets,
        ..
    } = &mut *network;
    let state = tcp_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    update(&mut state.options);
    for entry in &state.handles {
        if entry.passive.is_none() {
            state
                .options
                .apply_tcp(sockets.get_mut::<tcp::Socket<'static>>(entry.handle));
        }
    }
    Ok(())
}

/// 只读实际 transport 的测试观察 seam；不改变 policy、队列或连接状态。
#[cfg(test)]
pub(super) fn transport_options_match(
    network: &NetworkStack,
    id: usize,
    no_delay: bool,
    hop_limit: Option<u8>,
) -> bool {
    let state = &network.tcp_endpoints[&id];
    !state.handles.is_empty()
        && state
            .handles
            .iter()
            .filter(|entry| entry.passive.is_none())
            .all(|entry| {
                let actual = network.sockets.get::<tcp::Socket<'static>>(entry.handle);
                actual.nagle_enabled() != no_delay && actual.hop_limit() == hop_limit
            })
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

/// 绑定 fresh TCP endpoint；port 0 经唯一 allocator 分配 ephemeral port。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `address`: 请求的 IPv4 address 与 port。
///
/// # Returns
///
/// 成功取得本地 endpoint 后返回 unit。
///
/// # Errors
///
/// 返回地址、状态、冲突或分配错误。
pub(super) fn bind(socket: &InetSocket, address: InetAddress) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let address_filter = (!address.address.is_unspecified()).then_some(address.address);
    if address_filter.is_some_and(|candidate| !network.has_local_address(candidate)) {
        return Err(SocketError::AddressNotAvailable);
    }
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !matches!(state.mode, TcpMode::Closed { bound: None, .. }) {
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
    let TcpMode::Closed { shutdown_read, .. } = state.mode else {
        unreachable!()
    };
    state.mode = TcpMode::Closed {
        bound: Some(listen_endpoint(InetAddress {
            port: lease.port(),
            ..address
        })),
        shutdown_read,
    };
    state.port_lease = Some(lease);
    Ok(())
}

/// 将 fresh TCP endpoint 原子转换为有界 passive listener。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `backlog`: 请求深度，截断到文档声明的 kernel 上限。
///
/// # Returns
///
/// 全部 listen handle 就绪后返回 unit。
///
/// # Errors
///
/// 返回状态、地址或分配错误，且不会发布半初始化 listener。
pub(super) fn listen(socket: &InetSocket, backlog: usize) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let (bound, reuse_address) = match state.mode {
        TcpMode::Closed { bound, .. } => (bound, state.options.reuse_address),
        TcpMode::Listening { .. } => return Ok(()),
        _ => return Err(SocketError::Invalid),
    };
    let backlog = backlog.clamp(1, TCP_BACKLOG_MAX);
    lifecycle::prepare_closed_transport(&mut network, id)?;
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
    let first = network.tcp_endpoints[&id].handles[0].handle;
    let send_mss = network
        .sockets
        .get::<tcp::Socket<'static>>(first)
        .send_mss();
    let options = network.tcp_endpoints[&id].options;
    for handle in core::iter::once(first).chain(extra.iter().copied()) {
        options.apply_tcp(network.sockets.get_mut::<tcp::Socket<'static>>(handle));
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
    state.handles.extend(extra.into_iter().map(TcpHandle::new));
    state.mode = TcpMode::Listening {
        endpoint,
        backlog,
        send_mss,
    };
    state.port_lease = Some(listener_lease);
    Ok(())
}

/// 通过唯一 interface context 启动 active TCP handshake。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `peer`: 远端 IPv4 endpoint。
///
/// # Returns
///
/// SYN 提交后返回 `InProgress`；完成状态通过 readiness 观察。
///
/// # Errors
///
/// 返回标准地址、route、状态或 in-progress 错误。
pub(super) fn connect(socket: &InetSocket, peer: InetAddress) -> Result<(), SocketError> {
    if peer.port == 0 || peer.address.is_unspecified() {
        return Err(SocketError::AddressNotAvailable);
    }
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let (bound, reuse_address, existing_lease) = match network.tcp_endpoints.get(&id) {
        Some(state) => match state.mode {
            TcpMode::Closed { bound, .. } => (bound, state.options.reuse_address, state.port_lease),
            TcpMode::Connecting { terminal: true, .. } => {
                return Err(finish_failed_connect(&mut network, id));
            }
            TcpMode::Connecting { .. } => return Err(SocketError::AlreadyInProgress),
            TcpMode::Connected(_) => return Err(SocketError::AlreadyConnected),
            TcpMode::Listening { .. } => return Err(SocketError::Invalid),
        },
        None => return Err(SocketError::NotConnected),
    };
    let route = network.route(
        peer.address,
        network.tcp_endpoints[&id].options.bound_to_device,
    )?;
    let interface_index = route.interface_index;
    let route_address = network.source_address(route);
    let local_address = bound
        .and_then(|endpoint| endpoint.addr)
        .map(from_ip)
        .or(route_address)
        .ok_or(SocketError::NetworkUnreachable)?;
    lifecycle::prepare_closed_transport(&mut network, id)?;
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
    let handle = network.tcp_endpoints[&id].handles[0].handle;
    let NetworkStack {
        interfaces,
        sockets,
        ..
    } = &mut *network;
    let interface = &mut interfaces
        .iter_mut()
        .find(|interface| interface.index == interface_index)
        .expect("selected actual TCP device disappeared")
        .interface;
    interface.context().set_egress_route(
        ipv4(route.next_hop),
        route.logical_interface_index,
        false,
        Some(route.transport),
    );
    let connect = sockets.get_mut::<tcp::Socket<'static>>(handle).connect(
        interface.context(),
        IpEndpoint::new(ipv4(peer.address), peer.port),
        IpListenEndpoint {
            addr: Some(ipv4(local_address)),
            ..local
        },
    );
    interface.context().clear_egress_route();
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
    state.mode = TcpMode::Connecting {
        bound: local,
        terminal: false,
        shutdown_read: false,
        shutdown_write: false,
        local: InetAddress {
            address: local_address,
            port: local.port,
        },
    };
    state.pending_error = None;
    state.port_lease = Some(connected_lease);
    drop(network);
    crate::drivers::network::request_poll();
    Err(SocketError::InProgress)
}

/// 读取权威 local TCP endpoint。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// bound/listening/connected 或 unspecified 本地地址。
///
/// # Errors
///
/// endpoint 删除后返回 `NotConnected`。
pub(super) fn address(socket: &InetSocket) -> Result<InetAddress, SocketError> {
    let id = endpoint_id(socket);
    let network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    match state.mode {
        TcpMode::Connecting { local, .. } => Ok(local),
        TcpMode::Connected(connection) => Ok(connection.local),
        TcpMode::Closed { bound, .. } => Ok(bound.map_or(
            InetAddress {
                address: Ipv4Addr::UNSPECIFIED,
                port: 0,
            },
            |endpoint| InetAddress {
                address: endpoint.addr.map_or(Ipv4Addr::UNSPECIFIED, from_ip),
                port: endpoint.port,
            },
        )),
        TcpMode::Listening { endpoint, .. } => Ok(InetAddress {
            address: endpoint.addr.map_or(Ipv4Addr::UNSPECIFIED, from_ip),
            port: endpoint.port,
        }),
    }
}

/// 读取权威 connected TCP peer endpoint。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// 远端 IPv4 endpoint。
///
/// # Errors
///
/// tuple 尚未建立或 endpoint 已删除时返回 `NotConnected`。
pub(super) fn peer_address(socket: &InetSocket) -> Result<InetAddress, SocketError> {
    let id = endpoint_id(socket);
    let network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let TcpMode::Connected(connection) = state.mode else {
        return Err(SocketError::NotConnected);
    };
    let phase = network
        .sockets
        .get::<tcp::Socket<'static>>(state.handles[0].handle)
        .state();
    if matches!(phase, State::Closed | State::SynSent) {
        return Err(SocketError::NotConnected);
    }
    Ok(connection.peer)
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
pub(super) fn connection_result(socket: &InetSocket) -> Result<(), SocketError> {
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
            let phase = network
                .sockets
                .get::<tcp::Socket<'static>>(state.handles[0].handle)
                .state();
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
            network
                .tcp_ports
                .restore_bound_address(lease, bound.addr.map(from_ip)),
        );
    }
    error
}
