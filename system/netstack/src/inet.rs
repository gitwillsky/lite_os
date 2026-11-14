use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use crate::sync::Mutex;
use ::tcp::time::Instant;

use crate::{namespace::Namespace, packet, receive::ReceiveBuffer, time::get_time_us};
use fallible_tree::FallibleMap;
use native_runtime::{
    Event,
    native_abi::{self, Status},
};
use router_protocol::connections::{Cursor, Identity, Page, changes};

use self::interface::NetworkInterface;
use self::options::InetSocketOptions;
use self::port_namespace::{PortBinding, PortError, PortLease, PortNamespace};
pub(crate) use self::protocol_owner::NetworkStackOwner;
use self::tcp::TcpEndpointState;
use network_protocol::{
    InetAddress, InetFamily, InetProtocol, InternetOption, InternetPacketInfo,
    InternetSendPacketInfo, SocketError,
};

mod arp;
mod configuration;
mod datagram;
mod device;
mod error_queue;
mod firewall;
mod forward;
mod icmp_error;
mod identification;
mod ingress;
mod interface;
mod ipv6;
mod multicast;
mod neighbor;
mod options;
mod path_mtu;
mod poll;
mod port_namespace;
mod protocol_owner;
mod raw;
mod readiness;
mod reassembly;
mod route;
mod route_reply;
mod switching;
mod tcp;
mod timing;
mod udp;
mod wait;
pub(crate) use configuration::{
    ConfigurationSession, address_snapshot, configure_address, configure_broadcast, configure_mtu,
    configure_netmask, configure_route, configure_up, inject_virtual_packet, install_interface,
    interface_index, interface_snapshot, interface_snapshots, learned_dns, neighbor_page,
    neighbor_snapshot, network_snapshot, observe_dns, observe_interfaces, packet_interface,
    packet_transmit_available, prepare_configuration, prepare_next_hop, project_bridges,
    release_interface, route_lookup, route_snapshot, route_snapshots, routing_inputs,
    rule_snapshot,
};
pub(crate) use firewall::counter_page as firewall_counters;
pub(crate) use route::route_page;
pub(crate) use timing::network_poll_deadline;

// 每轮最多消费 64 个 frame，避免持续 RX 流量让当前 CPU 永久停留在 softirq context；
// 若没有此上限，user task 和其他 deferred work 在高包速下可能饥饿。
const NETWORK_RX_BUDGET: usize = 64;
// IPv4 的当前默认 TTL；NETNS IPv4 sysctl 接入时从实际 namespace policy 取得。
const DEFAULT_IPV4_HOP_LIMIT: u8 = 64;
// Readiness handoff 与 RX/TX 使用同一 softirq 上限；满批次会重新投递 Network bit。
// 缺少此上限会让大量 endpoint edge 在一次 safe point 内无界进入 wait owner。
const NETWORK_NOTIFICATION_BUDGET: usize = 64;
// 本 NET 原生 TCP slot、UDP 与 RAW endpoint 的统一准入上限；限制冷路径登记和活跃协议内存。
const SOCKET_STORAGE_CAPACITY: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct InterfaceState {
    address: Option<Ipv4Addr>,
    prefix_length: u8,
    // OWNER: native ifa_broadcast，允许明确配置非默认地址；不能由每次getter按mask重建。
    broadcast: Ipv4Addr,
    up: bool,
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

struct NetworkStack {
    domain: u64,
    // OWNER: 只在被观测的执行域装配一次的变化通知，不镜像接口状态；写入时通知，报文路径不检查它。
    interface_observer: Option<Arc<Event>>,
    // 配置服务的唯一 DNS 通知源，重连时替换。DNS 事实仍只在各接口拥有。
    dns_observer: Option<Event>,
    // 严格按 ifindex 递增，链路与选路借用同一张设备表。
    interfaces: Vec<NetworkInterface>,
    // OWNER: 本NET唯一IPv4 FIB，自动地址route和用户route共享配置/查找/退休事务。
    routing: route::RoutingTable,
    // OWNER: 本 NET 的唯一双栈路径 MTU 估计；IP 层共享，socket 仅保存显式发送上限，不复制学习值。
    path_mtu: path_mtu::PathMtu,
    // OWNER: 本 NET 唯一双栈重组表；只有已通过本机目的准入的实际分片分配存储，超时/移除完整退休。
    reassembly: reassembly::Reassembly,
    // OWNER: IPv6 本机/显式直连前缀的唯一执行根；查询资格来自地址 owner，事务只交换预备根。
    ipv6_routing: route::Ipv6Routes,
    // OWNER: 当前 FIB 事务要求继续复探的已失败下一跳。实际可达性和时钟仍只在邻居表。
    gateway_probes: Vec<(u32, core::net::IpAddr)>,
    // OWNER: 选表规则随执行 FIB 在同一 NET guard 交换；报文和重试只能借用当代规则。
    selection_rules: route::SelectionRules,
    firewall: firewall::Firewall,
    switching: switching::Switching,
    // OWNER: 唯一在途配置；冷路径预备时分配，确认/回滚后释放。持有期间拒绝其他配置写入，
    // 否则逆向节点会覆盖并发提交；数据包与 socket 操作不取得这项配置租约。
    configuration: Option<alloc::boxed::Box<configuration::Transaction>>,
    // OWNER: protocol即时IP回复只有此有预算queue，ARP仍是实际link内响应。
    route_replies: route_reply::RouteReplies,
    forwarding: forward::Forwarding,
    multicast_routes: forward::multicast::Table,
    // OWNER: 本域唯一实际 IPv4 转发策略，由授权配置事务交换；关闭只影响转发，不影响本机协议或路由学习。
    forwarding_enabled: network_protocol::Forwarding,
    // OWNER: 本 NET 的非零主机 IPv4 packet ID；HDRINCL 的零 ID 在发送准入时分配，不另建全局计数。
    identification: identification::Identification,
    // OWNER: 8-interface 窗口与预算耗尽后的续接位置，防止持续 lo/单 NIC 流量饿死其他设备。
    interface_cursor: usize,
    interface_rotation: usize,
    // OWNER: 本次接口遍历至少有一个批次在排空前停下。跨窗口保留，防止尾部设备为空时丢失续调度。
    interface_revisit: bool,
    // OWNER: 下一个以太网 ifindex（回环为 1）；单调递增、移除后不复用。缺失时按接口数分配会让
    // 新网卡继承已移除接口的序号，旧 SO_BINDTODEVICE 与 packet 绑定被静默转到新设备。
    next_ethernet_index: u32,
    udp_endpoints: FallibleMap<u64, udp::EndpointState>,
    raw_endpoints: FallibleMap<u64, raw::RawEndpointState>,
    tcp_endpoints: FallibleMap<usize, TcpEndpointState>,
    // OWNER: 本 NET 的 ISN PRF 密钥展开状态，只供打开连接；不公开、记录或在网络域之间复用。
    syn_sequence_key: tcp::SequenceKey,
    udp_ports: PortNamespace,
    tcp_ports: PortNamespace,
    next_tcp_id: usize,
    // OWNER: TCP 输出轮转位置；缺失时持续满窗口的首个连接会占尽每轮提交预算。
    tcp_cursor: usize,
    // OWNER: 实际 UDP endpoint TX 轮转位置；不保存队列或 readiness 镜像。
    udp_cursor: u64,
    // OWNER: RAW 输出的轮转位置；持续发送者不能占尽所有 endpoint 的本轮预算。
    raw_cursor: u64,
}

fn stack(scope: &Namespace) -> Result<&NetworkStackOwner, SocketError> {
    scope.protocol().ok_or(SocketError::NetworkUnreachable)
}

fn port_error(error: PortError) -> SocketError {
    match error {
        PortError::AddressInUse => SocketError::AddressInUse,
        PortError::InvalidBinding => SocketError::Invalid,
        PortError::NoMemory => SocketError::NoMemory,
    }
}

fn now() -> Instant {
    Instant::from_millis((get_time_us() / 1000) as i64)
}

impl NetworkStack {
    // 仅用于冷准入：实际 TCP 机制直接属于 endpoint，不维护一份易漏回收的计数镜像。
    fn socket_count(&self) -> usize {
        self.raw_endpoints.len()
            + self
                .tcp_endpoints
                .values()
                .map(TcpEndpointState::slot_count)
                .sum::<usize>()
    }
}

/// 在 publication 前准备实际双族网络域；全部fallible allocation不留下注册/端口副作用。
/// 运行期的网卡由安装协议逐块加入（[`install_interface`]）；`devices` 是没有安装者的初始网卡
/// （eth{n}、ifindex n+2），不随会话关闭移除。
pub(crate) fn prepare_network(
    scope: &Namespace,
    devices: &[Arc<dyn device_model::network::NetworkDevice>],
    loopback_up: bool,
) -> Result<Arc<NetworkStackOwner>, SocketError> {
    let mut interfaces = Vec::new();
    interfaces
        .try_reserve_exact(devices.len().checked_add(1).ok_or(SocketError::NoMemory)?)
        .map_err(|_| SocketError::NoMemory)?;
    interfaces.push(NetworkInterface::loopback(scope, loopback_up)?);
    for (ordinal, device) in devices.iter().enumerate() {
        let unit = u32::try_from(ordinal).map_err(|_| SocketError::NoSpace)?;
        let index = unit.checked_add(2).ok_or(SocketError::NoSpace)?;
        interfaces.push(NetworkInterface::ethernet(
            scope,
            index,
            unit,
            device.clone(),
            None,
        )?);
    }
    let next_ethernet_index =
        u32::try_from(interfaces.len() + 1).map_err(|_| SocketError::NoSpace)?;
    let mut routing = route::RoutingTable::new();
    for interface in &interfaces {
        routing.replace_interface(routing.prepare_interface(
            interface.index,
            interface.state,
            interface.state,
            &interfaces,
            false,
        )?);
    }
    let ipv6_routing = route::Ipv6Routes::prepare(interfaces.iter(), &[], None, None)?;
    let owner = Arc::try_new(NetworkStackOwner::new(NetworkStack {
        domain: scope.id(),
        interface_observer: None,
        dns_observer: None,
        interfaces,
        routing,
        path_mtu: path_mtu::PathMtu::new(),
        reassembly: reassembly::Reassembly::new(),
        ipv6_routing,
        gateway_probes: Vec::new(),
        selection_rules: route::SelectionRules::default(),
        firewall: firewall::Firewall::default(),
        switching: switching::Switching::new(),
        configuration: None,
        route_replies: route_reply::RouteReplies::new(),
        forwarding: forward::Forwarding::new(),
        multicast_routes: Default::default(),
        forwarding_enabled: network_protocol::Forwarding {
            ipv4: router_protocol::configuration::forwarding_default(),
            ipv6: router_protocol::configuration::forwarding_default(),
        },
        identification: identification::Identification::new(),
        interface_cursor: 0,
        interface_rotation: 0,
        interface_revisit: false,
        next_ethernet_index,
        udp_endpoints: FallibleMap::new(),
        udp_cursor: 0,
        raw_cursor: 0,
        raw_endpoints: FallibleMap::new(),
        tcp_endpoints: FallibleMap::new(),
        syn_sequence_key: tcp::sequence_key(),
        udp_ports: PortNamespace::new(),
        tcp_ports: PortNamespace::new(),
        next_tcp_id: 1,
        tcp_cursor: 0,
    }))
    .map_err(|_| SocketError::NoMemory)?;
    if !devices.is_empty() {
        let activity = owner.link_activity(scope.id(), || ())?;
        for device in devices {
            device.bind_link_activity(activity.clone());
        }
    }
    Ok(owner)
}

/// 捕获所选域的一页连接；编码与 IPC 均在释放 NET guard 之后执行。
/// # Errors
/// 网络域不可用、游标属于另一执行实例或分页缓冲分配失败。
pub(crate) fn connection_page(
    scope: &crate::namespace::Namespace,
    cursor: Cursor,
) -> Result<Page, Status> {
    let network = stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    network.connections_at(cursor, now())
}

/// 读取同域连接增量，并在首次观察时建立有界历史。
/// # Errors
/// 网络域不可用或分配失败。
pub(crate) fn connection_changes(
    scope: &crate::namespace::Namespace,
    cursor: changes::Cursor,
) -> Result<changes::Batch, Status> {
    stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?
        .connection_changes_after(cursor)
}

/// 仅在客户端已读到末尾时返回等待能力。锁内核对游标，锁外才等待。
/// # Errors
/// 网络域不可用或等待能力创建失败。
pub(crate) fn arm_connection_watch(
    scope: &crate::namespace::Namespace,
    cursor: changes::Cursor,
    create: impl FnOnce() -> Result<Arc<Event>, Status>,
) -> Result<Option<Arc<Event>>, Status> {
    stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?
        .arm_connection_watch(cursor, create)
}

/// 在指定域清除一条活动连接及其尚未发出的报文。
/// # Errors
/// 网络域不可用，或指定身份已失效。
pub(crate) fn clear_connection(
    scope: &crate::namespace::Namespace,
    identity: Identity,
) -> Result<(), Status> {
    let mut network = stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    network.clear_connection_at(identity, now())
}

/// 本轮推进的后续工作。积压可立即继续，争用需要在释放已取得的协议锁后重试。
pub(crate) struct NetworkWork {
    /// 域、收发或清理仍有已就绪工作。
    pub(crate) backlog: bool,
    /// 某个域或目录本轮因争用未能推进。
    pub(crate) blocked: bool,
}

/// 每批最多服务八个域。首个域保证推进，其后只在单调时钟的微秒截止 `work_until` 之前开始。
///
/// # Returns
///
/// 有实际积压时立即续调度。域目录或 owner 竞争时报告跳过，由 worker 延后重试，避免争用时忙轮询。
///
/// # Errors
///
/// 不分配/等待；尚未装配protocol的准备域跳过，不影响其他实际域。
pub(crate) fn dispatch_network_work(
    scan: &mut crate::namespace::DomainScan,
    work_until: u64,
) -> NetworkWork {
    let Some(batch) = crate::namespace::take_network_namespaces(scan) else {
        return NetworkWork {
            backlog: false,
            blocked: true,
        };
    };
    let mut backlog = batch.backlog;
    let mut blocked = false;
    let mut last = None;
    for namespace in batch.namespaces.into_iter().flatten() {
        if let Some(domain) = last
            && get_time_us() >= work_until
        {
            scan.resume_after(domain);
            backlog = true;
            break;
        }
        last = Some(namespace.id());
        let packets = namespace.packets();
        let Some(stack) = namespace.protocol() else {
            backlog |= readiness::notify_packet_pending(packets);
            continue;
        };
        let Some(mut network) = stack.try_poll() else {
            blocked = true;
            backlog |= readiness::notify_packet_pending(packets);
            continue;
        };
        let cleanup_backlog = network.cleanup_backlog();
        let poll = network.poll_until(now(), work_until);
        // 设备会话关闭（驱动宿主终止或网卡移除）：同一 owner 事务内撤下接口与 routes。
        let mut removed = [None; 8];
        for (slot, failure) in removed.iter_mut().zip(&poll.device_failures) {
            if let Some((index, device_model::network::NetworkError::Disconnected)) = *failure
                && network.remove_interface(index)
            {
                *slot = Some(index);
            }
        }
        let notifications = network.take_pending_notifications();
        let notification_backlog = notifications.backlog();
        drop(network);
        readiness::notify_pending(notifications);
        for index in poll.packet_transmit_ready.into_iter().flatten() {
            packet::publish_transmit_ready(packets, index);
        }
        for index in removed.into_iter().flatten() {
            info!(
                "network namespace {} interface {} removed",
                namespace.id(),
                index
            );
        }
        for (index, failure) in poll.device_failures.into_iter().flatten() {
            if failure == device_model::network::NetworkError::Disconnected {
                continue;
            }
            error!(
                "network namespace {} interface {} adapter error: {:?}",
                namespace.id(),
                index,
                failure
            );
        }
        backlog |= poll.backlog || notification_backlog || cleanup_backlog;
        backlog |= readiness::notify_packet_pending(packets);
    }
    NetworkWork { backlog, blocked }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InetEndpoint {
    Udp(u64),
    Tcp(usize),
    Raw(u64),
}

/// Internet endpoint；UDP 使用双族 owner，TCP/RAW 当前为 IPv4，状态与地址均属于唯一 NetworkStack。
pub(crate) struct InetSocket {
    // 创建时所在的实际域；operation/cleanup/loan不得按其他域重新解析。
    namespace: Arc<Namespace>,
    // immutable dispatch key 不是 registration owner；closed observer 不能据此重新取得 slot。
    endpoint: InetEndpoint,
    // OWNER: operation guard 既串行化 payload loan，也独占唯一 protocol registration token。
    // None 是实际 closed phase；只有 take 能向 cleanup owner 交付 identity，Arc 不拥有打开阶段。
    operation: Mutex<Option<InetEndpoint>>,
    // 使用者等待的就绪 Event：状态可能变化时置位 `USER_0`，使用者清除后重新查询电平状态。
    readiness: Arc<Event>,
}

impl InetSocket {
    fn stack(&self) -> Result<&NetworkStackOwner, SocketError> {
        stack(&self.namespace)
    }
    /// 创建已验证的 UDP、TCP 或 RAW endpoint，并把协议状态注册到唯一 NetworkStack。
    ///
    /// # Parameters
    ///
    /// - `family`: 请求的 IP 地址族；尚未实现的协议组合明确拒绝。
    /// - `protocol`: request 已验证的传输类型。
    /// - `readiness`: endpoint 独占的就绪 Event。
    /// - `namespace`: endpoint 所在的域。
    ///
    /// # Returns
    ///
    /// 完整 InetSocket Arc。
    ///
    /// # Errors
    ///
    /// stack 不可用或协议 buffer 分配失败时返回错误。
    pub(crate) fn new(
        family: InetFamily,
        protocol: InetProtocol,
        readiness: Arc<Event>,
        namespace: Arc<Namespace>,
    ) -> Result<Arc<Self>, SocketError> {
        if protocol == InetProtocol::Tcp {
            let mut network = stack(&namespace)?.lock()?;
            let endpoint = try_allocate_endpoint(|| {
                let id = tcp::create_endpoint(&mut network, Weak::new(), family)?;
                Ok(Self {
                    namespace: namespace.clone(),
                    endpoint: InetEndpoint::Tcp(id),
                    operation: Mutex::new(Some(InetEndpoint::Tcp(id))),
                    readiness,
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
        if let InetProtocol::Raw(protocol) = protocol {
            return raw::new(family, protocol, readiness, namespace);
        }
        let mut network = stack(&namespace)?.lock()?;
        let endpoint = try_allocate_endpoint(|| {
            let handle = udp::create_endpoint(&mut network, family, Weak::new())?;
            Ok(Self {
                namespace: namespace.clone(),
                endpoint: InetEndpoint::Udp(handle),
                operation: Mutex::new(Some(InetEndpoint::Udp(handle))),
                readiness,
            })
        });
        let endpoint = endpoint?;
        let InetEndpoint::Udp(handle) = endpoint.endpoint else {
            unreachable!("UDP constructor returned a non-UDP endpoint")
        };
        network
            .udp_endpoints
            .get_mut(&handle)
            .expect("new UDP endpoint disappeared before Arc publication")
            .endpoint = Arc::downgrade(&endpoint);
        Ok(endpoint)
    }

    fn open_operation(
        &self,
    ) -> Result<crate::sync::MutexGuard<'_, Option<InetEndpoint>>, SocketError> {
        let operation = self.operation.lock();
        if operation.is_none() {
            return Err(SocketError::NotConnected);
        }
        Ok(operation)
    }

    /// 使用者关闭或未发布 endpoint rollback 消费同一 token；不由 observer Arc 决定时机。
    pub(crate) fn close_description(&self) {
        let retired = self.operation.lock().take();
        let Some(endpoint) = retired else {
            return;
        };
        // operation guard 覆盖全部 payload loan；take 后无新 loan，cleanup 不与真实 loan 并存。
        if let Some(stack) = self.namespace.protocol() {
            stack.cleanup_or_defer(self.namespace.id(), endpoint);
        }
    }

    fn udp_handle(&self) -> Result<u64, SocketError> {
        match self.endpoint {
            InetEndpoint::Udp(handle) => Ok(handle),
            InetEndpoint::Tcp(_) | InetEndpoint::Raw(_) => Err(SocketError::WrongType),
        }
    }

    pub(crate) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
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

    pub(crate) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::connect(self, peer);
        }
        if let InetEndpoint::Raw(id) = self.endpoint {
            return raw::connect(self, id, peer);
        }
        let handle = self.udp_handle()?;
        udp::connect(self, handle, peer)
    }

    /// AF_UNSPEC 取消 TCP 协议生命周期；在 operation/stack owner 释放后通知所有 waiter。
    pub(crate) fn disconnect(&self) -> Result<(), SocketError> {
        let operation = self.open_operation()?;
        let result = if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::disconnect(self)
        } else if let InetEndpoint::Udp(handle) = self.endpoint {
            udp::disconnect(self, handle)
        } else if let InetEndpoint::Raw(id) = self.endpoint {
            raw::disconnect(self, id)
        } else {
            Err(SocketError::OperationNotSupported)
        };
        drop(operation);
        self.notify();
        result
    }

    pub(crate) fn address(&self) -> Result<InetAddress, SocketError> {
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

    pub(crate) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::peer_address(self);
        }
        if let InetEndpoint::Raw(id) = self.endpoint {
            return raw::peer_address(self, id);
        }
        let handle = self.udp_handle()?;
        udp::peer_address(self, handle)
    }

    pub(crate) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: &[InternetSendPacketInfo],
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

    pub(crate) fn receive(
        &self,
        output: &mut ReceiveBuffer,
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
            return tcp::receive(self, output, peek).map(|count| (count, count, None, None));
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw::receive(self, handle, output, peek);
        }
        let handle = self.udp_handle()?;
        udp::receive(self, handle, output, peek)
    }

    /// 读取 TCP mode 的监听状态；UDP/RAW 为 false，不另存 policy flag。
    pub(crate) fn is_listening(&self) -> Result<bool, SocketError> {
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
    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
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
    /// - `readiness`: accepted endpoint 独占的就绪 Event。
    ///
    /// # Returns
    ///
    /// 持有 established handle 的新 endpoint。
    ///
    /// # Errors
    ///
    /// 非 listener、暂无连接或分配失败时返回错误。
    pub(crate) fn accept(&self, readiness: Arc<Event>) -> Result<Arc<Self>, SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::accept(self, readiness, self.namespace.clone())
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
    pub(crate) fn connection_result(&self) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::connection_result(self)
        } else {
            Ok(())
        }
    }

    /// 读取并清除 endpoint pending error。
    ///
    /// # Returns
    ///
    /// TCP/UDP/RAW pending error；读取后清除，无错误时为 None。
    pub(crate) fn take_error(&self) -> Option<SocketError> {
        let _operation = self.open_operation().ok()?;
        match self.endpoint {
            InetEndpoint::Tcp(_) => tcp::take_error(self),
            InetEndpoint::Udp(id) => udp::take_error(self, id),
            InetEndpoint::Raw(id) => raw::take_error(self, id),
        }
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
    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let operation = self.open_operation()?;
        let result = if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::shutdown(self, how)
        } else if let InetEndpoint::Udp(handle) = self.endpoint {
            udp::shutdown(self, handle, how)
        } else if let InetEndpoint::Raw(id) = self.endpoint {
            raw::shutdown(self, id, how)
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
