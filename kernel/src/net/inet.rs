use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::AnySocket,
    time::Instant,
    wire::IpAddress,
};
use spin::Mutex;

use crate::{
    devices::model::network_device, execution::timer::get_time_us, fallible_tree::FallibleMap,
    ipc::PipeEnd, ipc::ReceiveBuffer,
};

use self::interface::NetworkInterface;
use self::options::InetSocketOptions;
pub(crate) use self::options::InternetOption;
use self::port_namespace::{PortError, PortLease, PortNamespace};
use self::protocol_owner::NetworkStackGuard;
pub(super) use self::protocol_owner::NetworkStackOwner;
use self::tcp::TcpEndpointState;
use super::{
    InetAddress, InternetPacketInfo, InternetSendPacketInfo, SocketError, SocketPollState,
    SocketTimeouts, packet,
};

mod configuration;
mod device;
mod device_error;
mod interface;
mod options;
mod poll;
mod port_namespace;
mod protocol_owner;
mod raw;
mod readiness;
mod route;
mod route_reply;
mod tcp;
mod timing;
mod udp;
mod wait;
pub(crate) use configuration::{
    InterfaceSnapshot, RouteSnapshot, configure_address, configure_broadcast, configure_mtu,
    configure_netmask, configure_route, configure_up, interface_index, interface_snapshot,
    interface_snapshots, network_snapshot, route_snapshot,
};
pub(in crate::net) use configuration::{
    inject_loopback, packet_interface, packet_transmit_available,
};
pub(crate) use route::{Ipv4RouteError, Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest};
pub(crate) use timing::network_work_due;

// 每轮最多消费 64 个 frame，避免持续 RX 流量让当前 CPU 永久停留在 softirq context；
// 若没有此上限，user task 和其他 deferred work 在高包速下可能饥饿。
const NETWORK_RX_BUDGET: usize = 64;
// IPv4 的当前默认 TTL；NETNS IPv4 sysctl 接入时从实际 namespace policy 取得。
const DEFAULT_IPV4_HOP_LIMIT: u8 = 64;
// TX completion 与 RX 使用同一 softirq fairness 约束；缺失上限时大量完成的
// sender 可以在一次 user-return 前无界占用 deferred context。
const NETWORK_TX_COMPLETION_BUDGET: usize = 64;
// Readiness handoff 与 RX/TX 使用同一 softirq 上限；满批次会重新投递 Network bit。
// 缺少此上限会让大量 endpoint edge 在一次 safe point 内无界进入 wait owner。
const NETWORK_NOTIFICATION_BUDGET: usize = 64;
// smoltcp `SocketSet::add` 在 owned storage 耗尽时使用不可失败的
// `Vec::push`。该 owner 以默认 RLIMIT_NOFILE 作为单次启动的预留窗口；
// 缺失上限检查时 socket 压力会触发 kernel-wide allocation abort，而非 ENOMEM。
const SOCKET_STORAGE_CAPACITY: usize = 1024;

#[derive(Clone, Copy)]
struct InterfaceState {
    address: Option<Ipv4Addr>,
    prefix_length: u8,
    // OWNER: native ifa_broadcast，允许明确配置非默认地址；不能由每次getter按mask重建。
    broadcast: Ipv4Addr,
    up: bool,
}

struct EndpointState {
    endpoint: Weak<InetSocket>,
    // OWNER: Linux sk_userlocks 的实际 bind policy；地址/端口 tuple 仍只属于协议 socket。
    user_bind_locks: u8,
    // OWNER: native sk_shutdown 的唯一位集合；disconnect/connect 不清除，不作为库状态副本。
    shutdown: u8,
    options: InetSocketOptions,
    port_lease: Option<PortLease>,
    // 协议 poll 前的唯一 edge 快照；缺失时无法区分长期 writable 与新 writable transition。
    readiness: SocketPollState,
    // poll 已观察到 false → true，但尚未在 stack lock 外通知；缺失会迫使临界区内反向进入 wait owner。
    notification_pending: bool,
}

/// 在注册协议状态前分配 InetSocket 的 Arc storage。
///
/// `build` 只在 control block 已就绪后运行；缺失该顺序会让 endpoint 先进入 NetworkStack，
/// 随后的 Arc OOM 却无法向调用者返回一个未发布状态。
fn try_allocate_endpoint(
    build: impl FnOnce() -> Result<InetSocket, SocketError>,
) -> Result<Arc<InetSocket>, SocketError> {
    let mut slot = Arc::<InetSocket>::try_new_uninit().map_err(|_| SocketError::NoMemory)?;
    let endpoint = build()?;
    Arc::get_mut(&mut slot)
        .expect("new endpoint Arc must be uniquely owned")
        .write(endpoint);
    // SAFETY: slot 是尚未克隆的唯一 Arc，且上一行已完整初始化 InetSocket storage。
    Ok(unsafe { slot.assume_init() })
}

/// 已由 socket request 验证的 IPv4 endpoint 类型；RAW 也拥有实际协议队列。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InetProtocol {
    Udp,
    Tcp,
    Icmp,
    Raw,
}

struct NetworkStack {
    interfaces: Vec<NetworkInterface>,
    // OWNER: 本NET唯一IPv4 FIB，自动地址route和用户route共享配置/查找/退休事务。
    routing: route::RoutingTable,
    // OWNER: protocol即时IP回复只有此有预算queue，ARP仍是实际link内响应。
    route_replies: route_reply::RouteReplies,
    // OWNER: 本 NET 的非零 IPv4 packet ID；HDRINCL 的零 ID 在发送准入时分配，不另建全局计数。
    raw_packet_id: u16,
    // OWNER: 本轮egress route plan的预留scratch；每轮clear，无跨轮policy/ready cache。
    egress_routes: Vec<Option<route::RouteSelection>>,
    // OWNER: 8-interface批次与完整轮次后的起点轮换，防止持续lo/单NIC流量饿死其他设备。
    interface_cursor: usize,
    interface_rotation: usize,
    sockets: SocketSet<'static>,
    endpoints: FallibleMap<SocketHandle, EndpointState>,
    raw_endpoints: FallibleMap<SocketHandle, raw::RawEndpointState>,
    tcp_endpoints: FallibleMap<usize, TcpEndpointState>,
    udp_ports: PortNamespace,
    tcp_ports: PortNamespace,
    next_tcp_id: usize,
}

fn stack(scope: &super::NetNamespace) -> Result<&NetworkStackOwner, SocketError> {
    scope.protocol().ok_or(SocketError::NetworkUnreachable)
}

fn port_error(error: PortError) -> SocketError {
    match error {
        PortError::AddressInUse => SocketError::AddressInUse,
        PortError::NoMemory => SocketError::NoMemory,
    }
}

fn now() -> Instant {
    Instant::from_millis((get_time_us() / 1000) as i64)
}

fn ipv4(address: Ipv4Addr) -> IpAddress {
    IpAddress::Ipv4(address)
}

fn from_ip(address: IpAddress) -> Ipv4Addr {
    match address {
        IpAddress::Ipv4(address) => address,
    }
}

impl NetworkStack {
    fn add_socket<T: AnySocket<'static>>(
        &mut self,
        socket: T,
    ) -> Result<SocketHandle, SocketError> {
        if self.sockets.iter().count() >= SOCKET_STORAGE_CAPACITY {
            return Err(SocketError::NoMemory);
        }
        // init 已为全部 slot 预留 backing storage；active count 低于上限时，
        // add 要么复用 remove 留下的空洞，要么在已预留 capacity 内 push。
        Ok(self.sockets.add(socket))
    }
}

/// 在publication前准备一个实际IPv4 owner；全部fallible allocation不留下注册/端口副作用。
fn prepare_network(
    scope: &super::NetNamespace,
    devices: &[Arc<dyn crate::devices::model::network::NetworkDevice>],
    loopback_up: bool,
) -> Result<Arc<NetworkStackOwner>, SocketError> {
    let mut interfaces = Vec::new();
    interfaces
        .try_reserve_exact(devices.len().checked_add(1).ok_or(SocketError::NoMemory)?)
        .map_err(|_| SocketError::NoMemory)?;
    interfaces.push(NetworkInterface::loopback(scope.id(), loopback_up)?);
    for (ordinal, device) in devices.iter().enumerate() {
        interfaces.push(NetworkInterface::ethernet(
            scope.id(),
            u32::try_from(ordinal).map_err(|_| SocketError::NoSpace)?,
            device.clone(),
        )?);
    }
    let mut socket_storage = Vec::new();
    socket_storage
        .try_reserve_exact(SOCKET_STORAGE_CAPACITY)
        .map_err(|_| SocketError::NoMemory)?;
    let mut routing = route::RoutingTable::new();
    for interface in &interfaces {
        routing.replace_interface(
            interface.index,
            route::RoutingTable::prepare_interface(interface.index, interface.state)?,
        );
    }
    let mut egress_routes = Vec::new();
    egress_routes
        .try_reserve_exact(SOCKET_STORAGE_CAPACITY)
        .map_err(|_| SocketError::NoMemory)?;
    Arc::try_new(NetworkStackOwner::new(NetworkStack {
        interfaces,
        routing,
        route_replies: route_reply::RouteReplies::new(),
        raw_packet_id: 1,
        egress_routes,
        interface_cursor: 0,
        interface_rotation: 0,
        sockets: SocketSet::new(socket_storage),
        endpoints: FallibleMap::new(),
        raw_endpoints: FallibleMap::new(),
        tcp_endpoints: FallibleMap::new(),
        udp_ports: PortNamespace::new(),
        tcp_ports: PortNamespace::new(),
        next_tcp_id: 1,
    }))
    .map_err(|_| SocketError::NoMemory)
}

/// 标准NEWNET的完整准备：独立SocketSet/ports、实际DOWN且无地址的lo，没有旧域设备或配置。
/// 任一OOM释放未发表到Task的域及ucounts；所有消费者只在成功返回后移交membership。
pub(crate) fn create_net_namespace(
    user: Arc<crate::namespace::UserNamespace>,
    creator_uid: u32,
) -> Result<Arc<super::NetNamespace>, crate::namespace::NamespaceError> {
    let namespace = super::NetNamespace::create(user, creator_uid)?;
    let owner = prepare_network(&namespace, &[], false).map_err(|error| match error {
        SocketError::NoMemory => crate::namespace::NamespaceError::OutOfMemory,
        _ => crate::namespace::NamespaceError::Invalid,
    })?;
    namespace.install_protocol(owner);
    Ok(namespace)
}

/// 由composition root在device discovery后移交完整准备的初始IPv4 owner。
pub(crate) fn init() {
    let scope = super::net_namespace::initial_net_ref();
    if scope.protocol().is_some() {
        return;
    }
    // 学习型内核装配首张物理网卡和 lo；设备容器服务共同路径，不扩展多网卡支持目标。
    // 栈上 Option slice 避免额外 discovery allocation；没有 NIC 仍有真实回环协议。
    let device = network_device(0);
    let owner = prepare_network(scope, device.as_slice(), true);
    match owner {
        Ok(owner) => scope.install_protocol(owner),
        Err(_) => error!("network protocol owner allocation failed"),
    }
}

/// 在softirq context每批最多8个实际NET owner，有界推进RX/TX/cleanup与readiness。
///
/// # Returns
///
/// 域遍历、RX/cleanup/通知有backlog或owner竞争时返回true，caller回投下一批。
///
/// # Errors
///
/// 不分配/等待；尚未装配protocol的准备域跳过，不影响其他实际域。
pub(crate) fn dispatch_network_work() -> bool {
    use super::net_namespace::{NetworkNamespaceScan, take_network_namespaces};
    let Some(batch) = take_network_namespaces(NetworkNamespaceScan::Deferred) else {
        return true;
    };
    let mut backlog = batch.backlog;
    for namespace in batch.namespaces.into_iter().flatten() {
        let Some(stack) = namespace.protocol() else {
            continue;
        };
        let Some(mut network) = stack.try_poll() else {
            backlog = true;
            continue;
        };
        let cleanup_backlog = network.cleanup_backlog();
        let poll = network.poll();
        let notifications = network.take_pending_notifications();
        let notification_backlog = notifications.backlog();
        drop(network);
        readiness::notify_pending(notifications);
        for index in poll.packet_transmit_ready.into_iter().flatten() {
            packet::publish_transmit_ready(namespace.id(), index);
        }
        for (index, failure) in poll.device_failures.into_iter().flatten() {
            error!(
                "network namespace {} interface {} adapter error: {:?}",
                namespace.id(),
                index,
                failure
            );
        }
        backlog |= poll.backlog || notification_backlog || cleanup_backlog;
    }
    backlog |= readiness::notify_packet_pending();
    backlog
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InetEndpoint {
    Udp(SocketHandle),
    Tcp(usize),
    Raw(SocketHandle),
}

/// AF_INET UDP/TCP endpoint facade；协议状态和地址均保存在唯一 NetworkStack。
pub(super) struct InetSocket {
    // 与OFD facade共享实际memory/NET对象；operation/cleanup/loan不得按当前Task或initial域重解析。
    memory: Arc<super::SocketMemory>,
    // immutable dispatch key 不是 registration owner；closed observer 不能据此重新取得 slot。
    endpoint: InetEndpoint,
    // OWNER: operation guard 既串行化 payload loan，也独占唯一 protocol registration token。
    // None 是实际 closed phase；只有 take 能向 cleanup owner 交付 identity，Arc 不拥有打开阶段。
    operation: Mutex<Option<InetEndpoint>>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
    // 同一个实际 policy 的引用；stack registry 与 OFD 不保存需要人工同步的值。
    timeouts: Arc<SocketTimeouts>,
}

impl InetSocket {
    fn stack(&self) -> Result<&NetworkStackOwner, SocketError> {
        stack(self.memory.namespace_ref())
    }
    fn try_observe_stack(&self) -> Option<NetworkStackGuard<'_>> {
        self.memory.namespace_ref().protocol()?.try_observe()
    }
    /// 创建已验证的 UDP、TCP 或 RAW endpoint，并把协议状态注册到唯一 NetworkStack。
    ///
    /// # Parameters
    ///
    /// - `protocol`: request 已验证的 IPv4 endpoint 类型。
    /// - `notify`: endpoint 独占的 readiness notification Pipe。
    /// - `timeouts`: facade 预留的实际 policy，必须在注册 endpoint 前准备完成。
    ///
    /// # Returns
    ///
    /// 完整 InetSocket facade Arc。
    ///
    /// # Errors
    ///
    /// stack 不可用或协议 buffer 分配失败时返回错误。
    pub(super) fn new(
        protocol: InetProtocol,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        timeouts: Arc<SocketTimeouts>,
        memory: Arc<super::SocketMemory>,
    ) -> Result<Arc<Self>, SocketError> {
        if protocol == InetProtocol::Tcp {
            let mut network = stack(memory.namespace_ref())?.lock()?;
            let endpoint = try_allocate_endpoint(|| {
                let id = tcp::create_endpoint(&mut network, Weak::new(), timeouts.clone())?;
                Ok(Self {
                    memory: memory.clone(),
                    endpoint: InetEndpoint::Tcp(id),
                    operation: Mutex::new(Some(InetEndpoint::Tcp(id))),
                    notify_read: notify.0,
                    notify_write: notify.1,
                    timeouts,
                })
            });
            let endpoint = endpoint?;
            let InetEndpoint::Tcp(id) = endpoint.endpoint else {
                unreachable!("TCP constructor returned a non-TCP endpoint")
            };
            network
                .tcp_endpoints
                .get_mut(&id)
                .expect("new TCP endpoint disappeared before Arc publication")
                .endpoint = Arc::downgrade(&endpoint);
            return Ok(endpoint);
        }
        if matches!(protocol, InetProtocol::Icmp | InetProtocol::Raw) {
            return raw::new(
                if protocol == InetProtocol::Icmp {
                    1
                } else {
                    255
                },
                notify,
                timeouts,
                memory,
            );
        }
        let mut network = stack(memory.namespace_ref())?.lock()?;
        let endpoint = try_allocate_endpoint(|| {
            let handle = udp::create_endpoint(&mut network, Weak::new())?;
            Ok(Self {
                memory: memory.clone(),
                endpoint: InetEndpoint::Udp(handle),
                operation: Mutex::new(Some(InetEndpoint::Udp(handle))),
                notify_read: notify.0,
                notify_write: notify.1,
                timeouts,
            })
        });
        let endpoint = endpoint?;
        let InetEndpoint::Udp(handle) = endpoint.endpoint else {
            unreachable!("UDP constructor returned a non-UDP endpoint")
        };
        network
            .endpoints
            .get_mut(&handle)
            .expect("new UDP endpoint disappeared before Arc publication")
            .endpoint = Arc::downgrade(&endpoint);
        Ok(endpoint)
    }

    fn open_operation(&self) -> Result<spin::MutexGuard<'_, Option<InetEndpoint>>, SocketError> {
        let operation = self.operation.lock();
        if operation.is_none() {
            return Err(SocketError::NotConnected);
        }
        Ok(operation)
    }

    /// actual fput 或未发布 endpoint rollback 消费同一 token；不由 observer Arc 决定时机。
    pub(super) fn close_description(&self) {
        let retired = self.operation.lock().take();
        let Some(endpoint) = retired else {
            return;
        };
        // operation guard 覆盖全部 payload loan；take 后无新 loan，cleanup 不与真实 loan 并存。
        if let Some(stack) = self.memory.namespace_ref().protocol() {
            stack.cleanup_or_defer(endpoint);
        }
        self.notify_read.close();
        self.notify_write.close();
    }

    fn udp_handle(&self) -> Result<SocketHandle, SocketError> {
        match self.endpoint {
            InetEndpoint::Udp(handle) => Ok(handle),
            InetEndpoint::Tcp(_) | InetEndpoint::Raw(_) => Err(SocketError::WrongType),
        }
    }

    pub(super) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::bind(self, address);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw::bind(self, handle, address);
        }
        let handle = self.udp_handle()?;
        udp::bind(self, handle, address)
    }

    pub(super) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::connect(self, peer);
        }
        if matches!(self.endpoint, InetEndpoint::Raw(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        let handle = self.udp_handle()?;
        udp::connect(self, handle, peer)
    }

    /// AF_UNSPEC 取消 TCP 协议生命周期；在 operation/stack owner 释放后通知所有 waiter。
    pub(super) fn disconnect(&self) -> Result<(), SocketError> {
        let operation = self.open_operation()?;
        let result = if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::disconnect(self)
        } else if let InetEndpoint::Udp(handle) = self.endpoint {
            udp::disconnect(self, handle)
        } else {
            Err(SocketError::OperationNotSupported)
        };
        drop(operation);
        self.notify();
        result
    }

    pub(super) fn address(&self) -> Result<InetAddress, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::address(self);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw::address(self, handle);
        }
        let handle = self.udp_handle()?;
        udp::address(self, handle)
    }

    pub(super) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::peer_address(self);
        }
        if matches!(self.endpoint, InetEndpoint::Raw(_)) {
            return Err(SocketError::NotConnected);
        }
        let handle = self.udp_handle()?;
        udp::peer_address(self, handle)
    }

    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: Option<InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            // native TCP sendmsg 忽略已导入的 msg_name；连接身份只由实际握手确定。
            return tcp::send(self, input);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw::send(self, handle, input, target, packet_info);
        }
        let handle = self.udp_handle()?;
        udp::send(self, handle, input, target, packet_info)
    }

    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<
        (
            usize,
            usize,
            Option<InetAddress>,
            Option<InternetPacketInfo>,
        ),
        SocketError,
    > {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::receive(self, output, peek)
                .map(|(count, full, _, _)| (count, full, None, None));
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            let (count, full_length, source, local) = raw::receive(self, handle, output, peek)?;
            return Ok((count, full_length, Some(source), Some(local)));
        }
        let handle = self.udp_handle()?;
        udp::receive(self, handle, output, peek)
    }

    pub(super) fn packet_info(&self) -> bool {
        self.option(InternetOption::PacketInfo)
            .is_ok_and(|value| value != 0)
    }

    /// 读取 TCP mode 的监听状态；UDP/RAW 为 false，不另存 policy flag。
    pub(super) fn is_listening(&self) -> Result<bool, SocketError> {
        let _operation = self.open_operation()?;
        if let InetEndpoint::Tcp(id) = self.endpoint {
            let network = self.stack()?.lock()?;
            Ok(tcp::is_listening(&network, id))
        } else {
            Ok(false)
        }
    }

    /// 把 TCP endpoint 转换为 passive listener。
    ///
    /// # Parameters
    ///
    /// - `backlog`: 请求的 accept queue 深度。
    ///
    /// # Returns
    ///
    /// listener 完整发布后返回 unit。
    ///
    /// # Errors
    ///
    /// UDP、无效状态、地址或分配失败时返回错误。
    pub(super) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::listen(self, backlog)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }

    /// 接受一个 TCP connection，并转移到独立 InetSocket facade。
    ///
    /// # Parameters
    ///
    /// - `notify`: accepted endpoint 独占的 readiness notification Pipe。
    ///
    /// # Returns
    ///
    /// 持有 established handle 的新 endpoint。
    ///
    /// # Errors
    ///
    /// 非 listener、暂无连接或分配失败时返回错误。
    pub(super) fn accept(
        &self,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        timeouts: Arc<SocketTimeouts>,
        memory: Arc<super::SocketMemory>,
    ) -> Result<Arc<Self>, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::accept(self, notify, timeouts, memory)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }

    /// 读取 active TCP connect 的最终状态。
    ///
    /// # Returns
    ///
    /// established 返回 unit；UDP connect 立即视为成功。
    ///
    /// # Errors
    ///
    /// TCP 仍在进行、被拒绝或未连接时返回错误。
    pub(super) fn connection_result(&self) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            let result = tcp::connection_result(self);
            if !matches!(result, Err(SocketError::InProgress)) {
                self.consume_notify();
            }
            result
        } else {
            Ok(())
        }
    }

    /// 读取并清除 endpoint pending error。
    ///
    /// # Returns
    ///
    /// TCP pending error；UDP 或无错误时为 None。
    pub(super) fn take_error(&self) -> Option<SocketError> {
        let _operation = self.open_operation().ok()?;
        matches!(self.endpoint, InetEndpoint::Tcp(_))
            .then(|| tcp::take_error(self))
            .flatten()
    }

    /// 提交 TCP receive/send half-close。
    ///
    /// # Parameters
    ///
    /// - `how`: Linux `SHUT_RD/WR/RDWR` selector。
    ///
    /// # Returns
    ///
    /// 成功返回 unit。
    ///
    /// # Errors
    ///
    /// 未连接 UDP 与 closed TCP 返回 NotConnected，但仍提交实际 shutdown 并唤醒。
    pub(super) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let operation = self.open_operation()?;
        let result = if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::shutdown(self, how)
        } else if let InetEndpoint::Udp(handle) = self.endpoint {
            udp::shutdown(self, handle, how)
        } else {
            Err(SocketError::OperationNotSupported)
        };
        drop(operation);
        // SHUT_RD 没有协议 egress；必须直接唤醒已经睡眠的 recv/epoll waiter。
        self.notify();
        result
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        self.close_description();
    }
}

/// final InetSocket cleanup 的唯一 protocol-state mutation path。
fn cleanup_endpoint(network: &mut NetworkStack, endpoint: InetEndpoint) {
    match endpoint {
        InetEndpoint::Tcp(id) => tcp::drop_endpoint(network, id),
        InetEndpoint::Raw(handle) => raw::drop_endpoint(network, handle),
        InetEndpoint::Udp(handle) => udp::drop_endpoint(network, handle),
    }
}
