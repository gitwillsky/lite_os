use alloc::{sync::Weak, vec::Vec};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV6};
use network_protocol::{InetAddress, InetFamily};

use ::tcp::{self as transport, Endpoint as IpEndpoint, ListenEndpoint as IpListenEndpoint, State};
use smoltcp::time::Instant;

use fallible_tree::FallibleMap;

use super::{
    InetEndpoint, InetSocket, InetSocketOptions, NetworkStack, PortBinding, PortLease, SocketError,
    port_error,
};

mod accept;
mod authentication;
use authentication::{Binding, SharedKeys};
mod connect;
pub(super) use connect::{connect, connection_result};
mod io;
mod lifecycle;
mod storage;
pub(super) use accept::accept;
pub(super) use io::{
    maintain, maintain_clocks, poll_state, reap_orphans, receive, send, shutdown, take_error,
};
pub(super) use lifecycle::{disconnect, drop_endpoint};
use storage::{allocate_socket, placeholder_socket};
mod packet;
mod path;
pub(super) use packet::{ingress, ingress_ipv6, match_too_big};
use path::{Path, at};
pub(super) use path::{SequenceKey, sequence_key};

const TCP_BACKLOG_MAX: usize = 16;

#[derive(Clone, Copy)]
enum TcpMode {
    Closed {
        bound: Option<InetAddress>,
        shutdown_read: bool,
    },
    Connecting {
        local: InetAddress,
        bound: InetAddress,
        // SYN_RECV 半关闭仍属于未完成的实际握手；最终 ACK 才移交权限并发送 FIN。
        shutdown_read: bool,
        shutdown_write: bool,
        // 已交付一次实际 handshake failure；SO_ERROR 消费不重新生成错误。
        terminal: bool,
    },
    Connected(TcpConnection),
    Listening {
        endpoint: InetAddress,
        backlog: usize,
        // 实际 parent listener 的 mss_cache；每个 passive child 自己持协商 cache，不借 child 反推。
        send_mss: usize,
    },
}

/// 实际连接的地址与半关闭生命周期；协议库 tuple 仅负责路由，RST 清除它不释放绑定。
#[derive(Clone, Copy)]
struct TcpConnection {
    // 原始 bind identity 由生命周期 owner 保存；只在调用协议引擎时转换其类型，disconnect 恢复 user bind。
    bound: InetAddress,
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
    fn established(tcp: &transport::Socket<'_>, bound: InetAddress, family: InetFamily) -> Self {
        let local = tcp
            .local_endpoint()
            .expect("established TCP lost local tuple");
        let peer = tcp
            .remote_endpoint()
            .expect("established TCP lost peer tuple");
        Self {
            bound,
            local: present(
                family,
                InetAddress::new(local.addr, local.port),
                tcp.bound_interface(),
            ),
            peer: present(
                family,
                InetAddress::new(peer.addr, peer.port),
                tcp.bound_interface(),
            ),
            peer_closed: tcp.state() == State::CloseWait,
            shutdown_read: false,
            shutdown_write: false,
            terminal: false,
        }
    }

    /// 消费一次实际终止事件；FIN 后的合法 RST 仍产生 SO_ERROR，但读 EOF 优先。
    fn observe(&mut self, tcp: &mut transport::Socket<'_>) -> Option<SocketError> {
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
        if tcp.failure_reason() == Some(transport::FailureReason::TimedOut) {
            return Some(SocketError::TimedOut);
        }
        if tcp.failure_reason() == Some(transport::FailureReason::Reset) || !self.peer_closed {
            Some(SocketError::ConnectionReset)
        } else {
            None
        }
    }
}

/// 实际 passive child 的不可变握手快照；accept 后由新 endpoint 唯一保存 live policy。
struct PassivePolicy {
    options: InetSocketOptions,
    md5_keys: SharedKeys,
    connection: TcpConnection,
    pending_error: Option<SocketError>,
    // OWNER: 通常借 parent 覆盖；parent 改设备前转为独立 membership，accept 移交而非复制。
    port_lease: Option<PortLease>,
}

/// 一个实际原生连接及其尚未交付的 child policy；不另建平行 pending index。
struct TcpHandle {
    transport: transport::Socket<'static>,
    passive: Option<PassivePolicy>,
    md5: Option<Binding>,
    // OWNER: 本机制的最早输出重试时间；设备/邻居背压不能让持续 writable 的流忙轮询，实际事件可以提前重试。
    transmit_at: Instant,
}
impl TcpHandle {
    fn new(transport: transport::Socket<'static>) -> Self {
        Self {
            transport,
            passive: None,
            md5: None,
            transmit_at: Instant::from_millis(0),
        }
    }
}

/// NetworkStack 唯一拥有的 TCP endpoint lifecycle 与原生连接集合。
pub(super) struct TcpEndpointState {
    family: InetFamily,
    /// 只在释放 stack lock 后用于 readiness notification 的 OFD-facing 弱引用。
    pub(super) endpoint: Weak<InetSocket>,
    handles: Vec<TcpHandle>,
    mode: TcpMode,
    pending_error: Option<SocketError>,
    pub(super) port_lease: Option<PortLease>,
    // 最后一个 facade 释放后保留 endpoint state，直到 TCP close 完成；若改用独立 orphan
    // queue，析构路径会因 queue 扩容而出现无法返回 ENOMEM 的失败点。
    orphaned: bool,
    /// 当前 endpoint 的 SOL_SOCKET policy；passive child 在握手完成时冻结，而非 accept 时复制。
    pub(super) options: InetSocketOptions,
    md5_keys: SharedKeys,
    // 协议 poll 前的唯一 edge 快照；缺失时长期 writable TCP 会持续唤醒全部 waiter。
    pub(super) readiness: network_protocol::SocketPollState,
    // 只跨越 stack unlock 保存一次 transition；缺失会在持 stack lock 时反向进入 wait owner。
    pub(super) notification_pending: bool,
}

impl TcpEndpointState {
    pub(super) fn is_ipv6(&self) -> bool {
        self.family == InetFamily::Ipv6
    }
    pub(super) fn slot_count(&self) -> usize {
        self.handles.len()
    }
}

#[cfg(test)]
impl TcpEndpointState {
    /// host 回归只观察实际 handle，不扩大 production lifecycle 字段可见性。
    pub(super) fn transport_for_test(&self) -> &transport::Socket<'static> {
        &self.handles[0].transport
    }
    pub(super) fn transport_for_test_mut(&mut self) -> &mut transport::Socket<'static> {
        &mut self.handles[0].transport
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

/// 注册 closed TCP handle 与唯一 endpoint state；流缓冲由首次 connect/listen 完整准备。
///
/// # Parameters
///
/// - `network`: 唯一 NetworkStack owner。
/// - `endpoint`: facade weak identity；调用方返回后立即发布完整 Arc。
///
/// # Returns
///
/// 稳定 TCP endpoint id。
///
/// # Errors
///
/// id、句柄或状态节点分配失败返回 `NoMemory`。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
    family: InetFamily,
) -> Result<usize, SocketError> {
    let endpoint_slot = FallibleMap::<usize, TcpEndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| SocketError::NoMemory)?;
    let id = allocate_endpoint_id(network)?;
    // CLOSED 没有可收发流量；提前分配两份 32 KiB 缓冲会让空闲 socket 消耗与活动连接相同的载荷空间。
    // 首次打开复用 prepare_closed_transport，只有双向缓冲完整准备后才替换此实际 slot。
    if network.socket_count() + network.udp_endpoints.len() >= super::SOCKET_STORAGE_CAPACITY {
        return Err(SocketError::NoMemory);
    }
    handles.push(TcpHandle::new(placeholder_socket()));
    network.tcp_endpoints.commit_vacant(endpoint_slot.fill(
        id,
        TcpEndpointState {
            family,
            endpoint,
            handles,
            mode: TcpMode::Closed {
                bound: None,
                shutdown_read: false,
            },
            pending_error: None,
            port_lease: None,
            orphaned: false,
            options: InetSocketOptions::default(),
            md5_keys: None,
            readiness: network_protocol::SocketPollState::error(),
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

/// IPv6 TCP policy 与原端口资格在同一 NET guard 更新；已绑定不能改变覆盖的地址族。
pub(super) fn set_ipv6_option(
    socket: &InetSocket,
    option: super::InternetOption,
    value: i32,
) -> Result<(), SocketError> {
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get_mut(&endpoint_id(socket))
        .ok_or(SocketError::NotConnected)?;
    if !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    match option {
        super::InternetOption::Ipv6MulticastAll => state.options.ipv6_multicast_all = value != 0,
        super::InternetOption::Ipv6Only if state.port_lease.is_none() => {
            state.options.ipv6_only = value != 0
        }
        super::InternetOption::Ipv6Only => return Err(SocketError::Invalid),
        super::InternetOption::Ipv6HopLimit if (-1..=255).contains(&value) => {
            state.options.ipv6_hop_limit = u8::try_from(value).ok()
        }
        super::InternetOption::Ipv6Mtu if value == 0 || value >= 1280 => {
            state.options.ipv6_mtu = (value != 0).then_some(value as u32)
        }
        super::InternetOption::Ipv6DontFragment => {
            // TCP 仍按 MSS 分段。保存标准 socket 选项供查询。
            state.options.ipv6_dont_fragment = value != 0;
        }
        super::InternetOption::Ipv6PacketInfo => return Err(SocketError::OperationNotSupported),
        _ => return Err(SocketError::Invalid),
    }
    for entry in &mut state.handles {
        if entry.passive.is_none() {
            state.options.apply_tcp(&mut entry.transport);
        }
    }
    Ok(())
}
/// 返回已关联 IPv6 TCP 的实际 IP 路径 MTU，不把 socket 手工上限冒充路径学习值。
pub(super) fn path_mtu(network: &NetworkStack, id: usize, ipv4: bool) -> Result<i32, SocketError> {
    i32::try_from(connected_output(network, id, ipv4)?.mtu).map_err(|_| SocketError::Invalid)
}

/// 按连接端点、绑定接口及标记解析本次 IP 路径。未关联或没有路径时返回领域错误。
pub(super) fn connected_output(network: &NetworkStack, id: usize, ipv4: bool) -> Result<super::route::Output, SocketError> {
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !ipv4 && !state.is_ipv6() {
        return Err(SocketError::OperationNotSupported);
    }
    let tcp = &state.handles[0].transport;
    let local = tcp.local_endpoint().ok_or(SocketError::NotConnected)?;
    let peer = tcp.remote_endpoint().ok_or(SocketError::NotConnected)?;
    if ipv4 && !peer.addr.is_ipv4() {
        return Err(SocketError::OperationNotSupported);
    }
    network.ip_output(
        local.addr,
        InetAddress::new(peer.addr, peer.port),
        (tcp.bound_interface(), state.options.mark),
        super::now(),
    )
}

pub(super) fn set_max_segment(socket: &InetSocket, value: Option<u16>) -> Result<(), SocketError> {
    update_transport_options(socket, |options| options.user_mss = value)
}

/// 原子更新设备 policy、端口占用及未冻结的 transport；已建立 child 保留原握手设备。
/// # Parameters
/// `network` 是独占 NET owner，`id` 为端点，`index` 已完成接口与权限验证。
/// # Returns
/// 更新全部提交后返回 unit；相同接口不产生存储工作。
/// # Errors
/// OOM 时撤销本次预留的 child memberships，不改变 parent 或已建立 child。
pub(super) fn bind_to_device(
    network: &mut NetworkStack,
    id: usize,
    index: Option<u32>,
) -> Result<(), SocketError> {
    let NetworkStack {
        tcp_endpoints,
        tcp_ports,
        ..
    } = network;
    let state = tcp_endpoints.get(&id).ok_or(SocketError::NotConnected)?;
    if state.options.bound_to_device == index {
        return Ok(());
    }
    // 1. 握手热路径不分配额外端口节点；只有 parent 改设备时，才为不再受其覆盖的 child 保留原绑定。
    // 2. 逐项准备并在同一 NET guard 内提交，后续项能复用已有节点；失败先释放临时 memberships。
    // 3. 所有可失败工作结束后，一次发布 parent、child policy 与实际 transport。
    let count = state
        .handles
        .iter()
        .filter(|entry| {
            entry
                .passive
                .as_ref()
                .is_some_and(|policy| policy.port_lease.is_none())
        })
        .count();
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(count)
        .map_err(|_| SocketError::NoMemory)?;
    let prepared = (|| {
        if let Some(parent) = state.port_lease {
            for (position, entry) in state.handles.iter().enumerate() {
                let Some(policy) = &entry.passive else {
                    continue;
                };
                if policy.port_lease.is_some() {
                    continue;
                }
                let binding =
                    PortBinding::new(policy.connection.local.ip(), policy.options.ipv6_only)?
                        .with_interface(policy.options.bound_to_device);
                let prepared = tcp_ports.prepare_retain_for_address(parent, binding)?;
                let lease = tcp_ports.commit_retained(prepared);
                let lease = tcp_ports.set_reuse(lease, policy.options.reuse_address);
                retained.push((position, lease));
            }
        }
        state
            .port_lease
            .map(|lease| tcp_ports.prepare_rebind(lease, lease.binding().with_interface(index)))
            .transpose()
    })();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            for (_, lease) in retained {
                tcp_ports.release(lease);
            }
            return Err(port_error(error));
        }
    };
    let lease = prepared.map(|prepared| tcp_ports.commit_readdress(prepared));
    let state = tcp_endpoints
        .get_mut(&id)
        .expect("TCP endpoint disappeared");
    state.options.bound_to_device = index;
    state.port_lease = lease;
    for (position, lease) in retained {
        state.handles[position]
            .passive
            .as_mut()
            .expect("retained child disappeared")
            .port_lease = Some(lease);
    }
    for entry in &mut state.handles {
        if entry.passive.is_none() {
            entry.transport.set_bound_interface(index);
        }
    }
    Ok(())
}

/// 读取唯一 TCP mode 的监听投影，供 SO_ACCEPTCONN 查询。
pub(super) fn is_listening(network: &NetworkStack, id: usize) -> bool {
    network
        .tcp_endpoints
        .get(&id)
        .is_some_and(|state| matches!(state.mode, TcpMode::Listening { .. }))
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
    state
        .handles
        .first()
        .map(|entry| entry.transport.max_segment_size() as i32)
        .ok_or(SocketError::NotConnected)
}

fn update_transport_options(
    socket: &InetSocket,
    update: impl FnOnce(&mut InetSocketOptions),
) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    update(&mut state.options);
    for entry in &mut state.handles {
        if entry.passive.is_none() {
            state.options.apply_tcp(&mut entry.transport);
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
                entry.transport.nagle_enabled() != no_delay
                    && entry.transport.hop_limit() == hop_limit
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
        addr: (!address.ip().to_canonical().is_unspecified()).then(|| address.ip().to_canonical()),
        port: address.port(),
    }
}

/// 绑定 fresh TCP endpoint；port 0 经唯一 allocator 分配 ephemeral port。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `address`: 请求的双族 address 与 port。
///
/// # Returns
///
/// 成功取得本地 endpoint 后返回 unit。
///
/// # Errors
///
/// 返回地址、状态、冲突或分配错误。
pub(super) fn bind(socket: &InetSocket, mut address: InetAddress) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    if !matches!(state.mode, TcpMode::Closed { bound: None, .. }) {
        return Err(SocketError::Invalid);
    }
    if (state.family == InetFamily::Ipv4) != address.is_ipv4() {
        return Err(SocketError::AddressFamilyNotSupported);
    }
    if state.options.ipv6_only && address.ip().to_canonical().is_ipv4() {
        return Err(SocketError::Invalid);
    }
    let mut device = state.options.bound_to_device;
    if let InetAddress::V6(v6) = address
        && v6.ip().is_unicast_link_local()
    {
        let index = (v6.scope_id() != 0)
            .then_some(v6.scope_id())
            .or(device)
            .ok_or(SocketError::Invalid)?;
        network
            .interface(index)
            .map_err(|_| SocketError::NoDevice)?;
        device = Some(index);
    }
    let ip = address.ip().to_canonical();
    if !ip.is_unspecified() && !network.has_local_ip(ip, device) {
        return Err(SocketError::AddressNotAvailable);
    }
    let binding = PortBinding::new(address.ip(), state.options.ipv6_only)
        .map_err(port_error)?
        .with_interface(device);
    let reuse = state.options.reuse_address;
    let lease = if address.port() == 0 {
        network.tcp_ports.acquire_ephemeral(binding, reuse)
    } else {
        network.tcp_ports.acquire(address.port(), binding, reuse)
    }
    .map_err(port_error)?;
    address.set_port(lease.port());
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .expect("bound TCP disappeared");
    let TcpMode::Closed { shutdown_read, .. } = state.mode else {
        unreachable!()
    };
    state.mode = TcpMode::Closed {
        bound: Some(present(state.family, address, device)),
        shutdown_read,
    };
    state.options.bound_to_device = device;
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
    let (bound, reuse_address) = match network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?
        .mode
    {
        TcpMode::Closed { bound, .. } => (bound, network.tcp_endpoints[&id].options.reuse_address),
        TcpMode::Listening { .. } => return Ok(()),
        _ => return Err(SocketError::Invalid),
    };
    let backlog = backlog.clamp(1, TCP_BACKLOG_MAX);
    lifecycle::prepare_closed_transport(&mut network, id)?;
    if network.socket_count() + network.udp_endpoints.len() + backlog - 1
        > super::SOCKET_STORAGE_CAPACITY
    {
        return Err(SocketError::NoMemory);
    }
    network
        .tcp_endpoints
        .get_mut(&id)
        .unwrap()
        .handles
        .try_reserve_exact(backlog - 1)
        .map_err(|_| SocketError::NoMemory)?;
    let mut extra = Vec::new();
    extra
        .try_reserve_exact(backlog - 1)
        .map_err(|_| SocketError::NoMemory)?;
    for _ in 1..backlog {
        extra.push(TcpHandle::new(allocate_socket()?));
    }
    let binding = PortBinding::new(
        unspecified(network.tcp_endpoints[&id].family, 0).ip(),
        network.tcp_endpoints[&id].options.ipv6_only,
    )
    .map_err(port_error)?
    .with_interface(network.tcp_endpoints[&id].options.bound_to_device);
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
    let endpoint = match (bound, new_lease) {
        (Some(endpoint), _) => endpoint,
        (None, Some(lease)) => unspecified(network.tcp_endpoints[&id].family, lease.port()),
        _ => unreachable!("TCP listener needs its bound or allocated port"),
    };
    let base_lease = new_lease.unwrap_or_else(|| {
        network.tcp_endpoints[&id]
            .port_lease
            .expect("bound TCP lost port lease")
    });
    let listener_lease = match network.tcp_ports.claim_listener(base_lease) {
        Ok(lease) => lease,
        Err(error) => {
            if let Some(lease) = new_lease {
                network.tcp_ports.release(lease);
            }
            return Err(port_error(error));
        }
    };
    let state = network.tcp_endpoints.get_mut(&id).unwrap();
    let send_mss = state.handles[0].transport.send_mss();
    for entry in state.handles.iter_mut().chain(extra.iter_mut()) {
        state.options.apply_tcp(&mut entry.transport);
        if entry.transport.listen(listen_endpoint(endpoint)).is_err() {
            state.handles[0].transport.abort();
            network.tcp_ports.release_listener_claim(listener_lease);
            if let Some(lease) = new_lease {
                network.tcp_ports.release(lease);
            }
            return Err(SocketError::AddressNotAvailable);
        }
    }
    state.handles.extend(extra);
    state.mode = TcpMode::Listening {
        endpoint,
        backlog,
        send_mss,
    };
    state.port_lease = Some(listener_lease);
    Ok(())
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
        TcpMode::Closed { bound, .. } => Ok(bound.unwrap_or_else(|| unspecified(state.family, 0))),
        TcpMode::Listening { endpoint, .. } => Ok(endpoint),
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
/// 远端双族 endpoint。
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
    let phase = state.handles[0].transport.state();
    if matches!(phase, State::Closed | State::SynSent) {
        return Err(SocketError::NotConnected);
    }
    Ok(connection.peer)
}

fn present(family: InetFamily, address: InetAddress, interface: Option<u32>) -> InetAddress {
    if family == InetFamily::Ipv4 {
        return InetAddress::new(address.ip().to_canonical(), address.port());
    }
    let ip = match address.ip() {
        IpAddr::V4(ip) => ip.to_ipv6_mapped(),
        IpAddr::V6(ip) => ip,
    };
    InetAddress::V6(SocketAddrV6::new(
        ip,
        address.port(),
        0,
        if ip.is_unicast_link_local() {
            interface.unwrap_or(0)
        } else {
            0
        },
    ))
}
fn unspecified(family: InetFamily, port: u16) -> InetAddress {
    InetAddress::new(
        match family {
            InetFamily::Ipv4 => Ipv4Addr::UNSPECIFIED.into(),
            InetFamily::Ipv6 => Ipv6Addr::UNSPECIFIED.into(),
        },
        port,
    )
}
