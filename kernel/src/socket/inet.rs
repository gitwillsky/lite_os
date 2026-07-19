use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::{Config, Interface, SocketHandle, SocketSet},
    socket::AnySocket,
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr},
};
use spin::Mutex;

use crate::{
    drivers::network::network_device, fallible_tree::FallibleMap, ipc::PipeEnd, ipc::ReceiveBuffer,
    timer::get_time_us,
};

use self::device::EthernetDevice;
use self::options::InetSocketOptions;
use self::port_namespace::{PortError, PortLease, PortNamespace};
use self::protocol_owner::{NETWORK_STACK, NetworkStackOwner};
use self::tcp::TcpEndpointState;
use super::{InetAddress, SocketError, SocketPollState, packet};

#[path = "inet/configuration.rs"]
mod configuration;
#[path = "device.rs"]
mod device;
#[path = "device_error.rs"]
mod device_error;
#[path = "inet/options.rs"]
mod options;
#[path = "inet/poll.rs"]
mod poll;
#[path = "inet/port_namespace.rs"]
mod port_namespace;
#[path = "inet/protocol_owner.rs"]
mod protocol_owner;
#[path = "inet/raw.rs"]
mod raw_endpoint;
#[path = "inet/readiness.rs"]
mod readiness;
#[path = "inet/tcp.rs"]
mod tcp;
#[path = "inet/timing.rs"]
mod timing;
#[path = "inet/udp.rs"]
mod udp_endpoint;
#[path = "inet/wait.rs"]
mod wait;
pub(crate) use configuration::{
    configure_address, configure_gateway, configure_netmask, configure_up, interface_snapshot,
    network_snapshot,
};
pub(crate) use timing::network_work_due;

// 每轮最多消费 64 个 frame，避免持续 RX 流量让当前 CPU 永久停留在 softirq context；
// 若没有此上限，user task 和其他 deferred work 在高包速下可能饥饿。
const NETWORK_RX_BUDGET: usize = 64;
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
    gateway: Option<Ipv4Addr>,
    up: bool,
}

struct EndpointState {
    endpoint: Weak<InetSocket>,
    peer: Option<InetAddress>,
    // IP_PKTINFO controls whether recvmsg publishes destination-address ancillary data. Keeping
    // it outside this endpoint owner would let setsockopt and packet delivery observe different flags.
    packet_info: bool,
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

struct NetworkStack {
    interface: Interface,
    device: EthernetDevice,
    sockets: SocketSet<'static>,
    endpoints: FallibleMap<SocketHandle, EndpointState>,
    raw_endpoints: FallibleMap<SocketHandle, raw_endpoint::RawEndpointState>,
    tcp_endpoints: FallibleMap<usize, TcpEndpointState>,
    udp_ports: PortNamespace,
    tcp_ports: PortNamespace,
    interface_state: InterfaceState,
    next_tcp_id: usize,
}

fn stack() -> Result<&'static NetworkStackOwner, SocketError> {
    let stack = NETWORK_STACK.get().ok_or(SocketError::NetworkUnreachable)?;
    // Device callback 不能通过 smoltcp trait 返回错误；由下一次 syscall seam 精确消费
    // 一次后恢复正常服务。只读取不清除会让单个畸形 completion 永久毒化整个网络栈。
    if let Some(error) = stack.lock()?.device.take_error() {
        return Err(network_error(error));
    }
    Ok(stack)
}

fn network_error(error: crate::drivers::network::NetworkError) -> SocketError {
    match error {
        crate::drivers::network::NetworkError::WouldBlock => SocketError::Again,
        crate::drivers::network::NetworkError::FrameTooLarge => SocketError::MessageTooLarge,
        crate::drivers::network::NetworkError::Device => SocketError::Device,
    }
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

    fn apply_interface_state(&mut self) {
        let state = self.interface_state;
        self.interface.update_ip_addrs(|addresses| {
            addresses.clear();
            if state.up
                && let Some(address) = state.address
            {
                addresses
                    .push(IpCidr::new(ipv4(address), state.prefix_length))
                    .expect("one IPv4 address must fit smoltcp interface storage");
            }
        });
        self.interface.routes_mut().remove_default_ipv4_route();
        if state.up
            && let Some(gateway) = state.gateway
        {
            self.interface
                .routes_mut()
                .add_default_ipv4_route(gateway)
                .expect("one default IPv4 route must fit smoltcp route storage");
        }
    }
}

/// @description 由 composition root 在 device discovery 后创建唯一 IPv4 stack。
pub(crate) fn init() {
    let Some(network_device) = network_device() else {
        return;
    };
    let mac = network_device.mac_address();
    let mut device = EthernetDevice::new(network_device);
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed =
        get_time_us() ^ u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
    let interface = Interface::new(config, &mut device, now());
    let mut socket_storage = Vec::new();
    if socket_storage
        .try_reserve_exact(SOCKET_STORAGE_CAPACITY)
        .is_err()
    {
        error!("network socket storage allocation failed");
        return;
    }
    NETWORK_STACK.call_once(|| {
        NetworkStackOwner::new(NetworkStack {
            interface,
            device,
            sockets: SocketSet::new(socket_storage),
            endpoints: FallibleMap::new(),
            raw_endpoints: FallibleMap::new(),
            tcp_endpoints: FallibleMap::new(),
            udp_ports: PortNamespace::new(),
            tcp_ports: PortNamespace::new(),
            interface_state: InterfaceState {
                address: None,
                prefix_length: 0,
                gateway: None,
                up: false,
            },
            next_tcp_id: 1,
        })
    });
}

/// @description 在 softirq context 有界推进 RX/TX、ARP、IPv4 与 UDP 状态。
///
/// @return RX budget 用尽且调用者必须重新投递 network softirq 时返回 `true`。
/// @errors stack 尚未初始化时返回 `false`，不产生错误。
pub(crate) fn dispatch_network_work() -> bool {
    if let Some(stack) = NETWORK_STACK.get() {
        let Some(mut network) = stack.try_poll() else {
            return true;
        };
        let cleanup_backlog = network.cleanup_backlog();
        let poll = network.poll();
        let notifications = network.take_pending_notifications();
        let notification_backlog = notifications.backlog();
        drop(network);
        readiness::notify_pending(notifications);
        if let Ok(poll) = poll {
            if poll.transmit_became_available {
                packet::publish_transmit_ready();
            }
            poll.backlog || notification_backlog || cleanup_backlog
        } else {
            notification_backlog || cleanup_backlog
        }
    } else {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InetEndpoint {
    Udp(SocketHandle),
    Tcp(usize),
    Raw(SocketHandle),
}

/// @description AF_INET UDP/TCP endpoint facade；协议状态和地址均保存在唯一 NetworkStack。
pub(super) struct InetSocket {
    endpoint: InetEndpoint,
    // OWNER: only this endpoint operation lock prevents two callers from borrowing the same
    // smoltcp socket. Making it global would restore cross-socket serialization.
    operation: Mutex<()>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl InetSocket {
    /// @description 创建 UDP 或 TCP endpoint，并把协议状态注册到唯一 NetworkStack。
    /// @param socket_type AF_INET datagram 或 stream 类型。
    /// @param notify endpoint 独占的 readiness notification Pipe。
    /// @return 完整 InetSocket facade Arc。
    /// @errors stack 不可用或协议 buffer 分配失败时返回错误。
    pub(super) fn new(
        socket_type: super::SocketType,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, SocketError> {
        if socket_type == super::SocketType::Stream {
            let mut network = stack()?.lock()?;
            let endpoint = try_allocate_endpoint(|| {
                let id = tcp::create_endpoint(&mut network, Weak::new())?;
                Ok(Self {
                    endpoint: InetEndpoint::Tcp(id),
                    operation: Mutex::new(()),
                    notify_read: notify.0,
                    notify_write: notify.1,
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
        if socket_type == super::SocketType::Raw {
            return raw_endpoint::new(notify);
        }
        let mut network = stack()?.lock()?;
        let endpoint = try_allocate_endpoint(|| {
            let handle = udp_endpoint::create_endpoint(&mut network, Weak::new())?;
            Ok(Self {
                endpoint: InetEndpoint::Udp(handle),
                operation: Mutex::new(()),
                notify_read: notify.0,
                notify_write: notify.1,
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

    fn udp_handle(&self) -> Result<SocketHandle, SocketError> {
        match self.endpoint {
            InetEndpoint::Udp(handle) => Ok(handle),
            InetEndpoint::Tcp(_) | InetEndpoint::Raw(_) => Err(SocketError::WrongType),
        }
    }

    pub(super) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::bind(self, address);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw_endpoint::bind(handle, address);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::bind(handle, address)
    }

    pub(super) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::connect(self, peer);
        }
        if matches!(self.endpoint, InetEndpoint::Raw(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::connect(handle, peer)
    }

    pub(super) fn address(&self) -> Result<InetAddress, SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::address(self);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw_endpoint::address(handle);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::address(handle)
    }

    pub(super) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::peer_address(self);
        }
        if matches!(self.endpoint, InetEndpoint::Raw(_)) {
            return Err(SocketError::NotConnected);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::peer_address(handle)
    }

    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
    ) -> Result<usize, SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            if target.is_some() {
                return Err(SocketError::AlreadyConnected);
            }
            return tcp::send(self, input);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw_endpoint::send(handle, input, target);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::send(handle, input, target)
    }

    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::receive(self, output, peek);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            let (count, full_length, source) = raw_endpoint::receive(self, handle, output, peek)?;
            return Ok((count, full_length, source, None));
        }
        let handle = self.udp_handle()?;
        udp_endpoint::receive(self, handle, output, peek)
    }

    pub(super) fn set_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
        let handle = self.udp_handle()?;
        udp_endpoint::set_packet_info(handle, enabled)
    }

    pub(super) fn packet_info(&self) -> bool {
        let _operation = self.operation.lock();
        let Ok(handle) = self.udp_handle() else {
            return false;
        };
        udp_endpoint::packet_info(handle)
    }

    /// @description 把 TCP endpoint 转换为 passive listener。
    /// @param backlog 请求的 accept queue 深度。
    /// @return listener 完整发布后返回 unit。
    /// @errors UDP、无效状态、地址或分配失败时返回错误。
    pub(super) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::listen(self, backlog)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }

    /// @description 接受一个 TCP connection，并转移到独立 InetSocket facade。
    /// @param notify accepted endpoint 独占的 readiness notification Pipe。
    /// @return 持有 established handle 的新 endpoint。
    /// @errors 非 listener、暂无连接或分配失败时返回错误。
    pub(super) fn accept(
        &self,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::accept(self, notify)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }

    /// @description 读取 active TCP connect 的最终状态。
    /// @return established 返回 unit；UDP connect 立即视为成功。
    /// @errors TCP 仍在进行、被拒绝或未连接时返回错误。
    pub(super) fn connection_result(&self) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
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

    /// @description 读取并清除 endpoint pending error。
    /// @return TCP pending error；UDP 或无错误时为 None。
    /// @errors 无错误。
    pub(super) fn take_error(&self) -> Option<SocketError> {
        let _operation = self.operation.lock();
        if let Some(stack) = NETWORK_STACK.get()
            && let Ok(network) = stack.lock()
            && let Some(error) = network.device.take_error()
        {
            return Some(network_error(error));
        }
        matches!(self.endpoint, InetEndpoint::Tcp(_))
            .then(|| tcp::take_error(self))
            .flatten()
    }

    /// @description 提交 TCP receive/send half-close。
    /// @param how Linux `SHUT_RD/WR/RDWR` selector。
    /// @return 成功返回 unit。
    /// @errors UDP 或未连接 TCP 返回错误。
    pub(super) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let _operation = self.operation.lock();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::shutdown(self, how)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        if let Some(stack) = NETWORK_STACK.get() {
            stack.cleanup_or_defer(self.endpoint);
        }
    }
}

/// final InetSocket cleanup 的唯一 protocol-state mutation path。
fn cleanup_endpoint(network: &mut NetworkStack, endpoint: InetEndpoint) {
    match endpoint {
        InetEndpoint::Tcp(id) => tcp::drop_endpoint(network, id),
        InetEndpoint::Raw(handle) => raw_endpoint::drop_endpoint(network, handle),
        InetEndpoint::Udp(handle) => udp_endpoint::drop_endpoint(network, handle),
    }
}
