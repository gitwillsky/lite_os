use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::{Config, Interface, PollIngressSingleResult, SocketHandle, SocketSet},
    socket::udp,
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr},
};
use spin::{Mutex, Once};

use crate::{
    drivers::network::{NetworkStatistics, network_device},
    ipc::{Pipe, PipeDirection, PipeEnd, PipeRead},
    timer::get_time_us,
};

use self::device::EthernetDevice;
use self::tcp::TcpEndpointState;
use super::{InetAddress, SocketError, SocketPollState};

#[path = "device.rs"]
mod device;
#[path = "inet/tcp.rs"]
mod tcp;
#[path = "inet/timing.rs"]
mod timing;
#[path = "inet/udp.rs"]
mod udp_endpoint;
pub(crate) use timing::network_work_due;

const EPHEMERAL_START: u16 = 49_152;
const EPHEMERAL_END: u16 = 65_535;
// 每轮最多消费 64 个 frame，避免持续 RX 流量让当前 hart 永久停留在 softirq context；
// 若没有此上限，user task 和其他 deferred work 在高包速下可能饥饿。
const NETWORK_RX_BUDGET: usize = 64;

struct PollOutcome {
    notifications: Vec<Arc<InetSocket>>,
    // 表示本轮未探测到 RX queue 为空；调用者必须重新投递 softirq，否则队列中已完成但
    // 没有新 IRQ edge 的 frame 可能永久滞留。
    rx_budget_exhausted: bool,
}

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
}

struct NetworkStack {
    interface: Interface,
    device: EthernetDevice,
    sockets: SocketSet<'static>,
    endpoints: BTreeMap<SocketHandle, EndpointState>,
    tcp_endpoints: BTreeMap<usize, TcpEndpointState>,
    orphaned_tcp: Vec<SocketHandle>,
    interface_state: InterfaceState,
    next_ephemeral: u16,
    next_tcp_ephemeral: u16,
    next_tcp_id: usize,
}

// OWNER: the IPv4 module uniquely owns interface configuration, routes, ARP cache, UDP socket set,
// endpoint peer state and ephemeral-port allocation. Duplicating any subset would make ioctl,
// packet dispatch and getsockname observe conflicting network identities.
static NETWORK_STACK: Once<Mutex<NetworkStack>> = Once::new();

fn stack() -> Result<&'static Mutex<NetworkStack>, SocketError> {
    NETWORK_STACK.get().ok_or(SocketError::NetworkUnreachable)
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
    fn poll(&mut self) -> PollOutcome {
        let before: Vec<_> = self
            .endpoints
            .iter()
            .map(|(handle, state)| {
                let socket = self.sockets.get::<udp::Socket<'static>>(*handle);
                (
                    *handle,
                    state.endpoint.clone(),
                    socket.can_recv(),
                    socket.can_send(),
                )
            })
            .collect();
        let tcp_before: Vec<_> = self
            .tcp_endpoints
            .iter()
            .map(|(&id, state)| (id, state.endpoint.clone(), state.poll_state(self)))
            .collect();
        let timestamp = now();
        // 1. 定时维护只执行一次，确保单轮协议推进的固定成本。
        self.interface.poll_maintenance(timestamp);
        // 2. ingress 逐帧推进并受 budget 限制，禁止网络洪泛独占当前 hart。
        let mut rx_budget_exhausted = true;
        for _ in 0..NETWORK_RX_BUDGET {
            if self
                .interface
                .poll_ingress_single(timestamp, &mut self.device, &mut self.sockets)
                == PollIngressSingleResult::None
            {
                rx_budget_exhausted = false;
                break;
            }
        }
        tcp::maintain(self);
        // 3. egress API 自身保证有界；在 ingress 后推进一次即可发送 ARP/UDP 响应。
        self.interface
            .poll_egress(timestamp, &mut self.device, &mut self.sockets);
        let mut notifications = Vec::new();
        for (handle, endpoint, was_readable, was_writable) in before {
            let socket = self.sockets.get::<udp::Socket<'static>>(handle);
            if (!was_readable && socket.can_recv() || !was_writable && socket.can_send())
                && let Some(endpoint) = endpoint.upgrade()
            {
                notifications.push(endpoint);
            }
        }
        for (id, endpoint, before) in tcp_before {
            if let Some(state) = self.tcp_endpoints.get(&id) {
                let after = state.poll_state(self);
                if after != before
                    && (after.readable || after.writable || after.hangup || after.error)
                    && let Some(endpoint) = endpoint.upgrade()
                {
                    notifications.push(endpoint);
                }
            }
        }
        PollOutcome {
            notifications,
            rx_budget_exhausted,
        }
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
    NETWORK_STACK.call_once(|| {
        Mutex::new(NetworkStack {
            interface,
            device,
            sockets: SocketSet::new(Vec::new()),
            endpoints: BTreeMap::new(),
            tcp_endpoints: BTreeMap::new(),
            orphaned_tcp: Vec::new(),
            interface_state: InterfaceState {
                address: None,
                prefix_length: 0,
                gateway: None,
                up: false,
            },
            next_ephemeral: EPHEMERAL_START,
            next_tcp_ephemeral: EPHEMERAL_START,
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
        let outcome = stack.lock().poll();
        for endpoint in outcome.notifications {
            endpoint.notify();
        }
        outcome.rx_budget_exhausted
    } else {
        false
    }
}

#[derive(Clone, Copy)]
enum InetEndpoint {
    Udp(SocketHandle),
    Tcp(usize),
}

/// @description AF_INET UDP/TCP endpoint facade；协议状态和地址均保存在唯一 NetworkStack。
pub(super) struct InetSocket {
    endpoint: InetEndpoint,
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
            let mut network = stack()?.lock();
            let id = tcp::create_endpoint(&mut network, Weak::new())?;
            let endpoint = Arc::new(Self {
                endpoint: InetEndpoint::Tcp(id),
                notify_read: notify.0,
                notify_write: notify.1,
            });
            network
                .tcp_endpoints
                .get_mut(&id)
                .expect("new TCP endpoint disappeared before Arc publication")
                .endpoint = Arc::downgrade(&endpoint);
            return Ok(endpoint);
        }
        let mut network = stack()?.lock();
        let handle = udp_endpoint::create_endpoint(&mut network, Weak::new())?;
        let endpoint = Arc::new(Self {
            endpoint: InetEndpoint::Udp(handle),
            notify_read: notify.0,
            notify_write: notify.1,
        });
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
            InetEndpoint::Tcp(_) => Err(SocketError::WrongType),
        }
    }

    pub(super) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::bind(self, address);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::bind(handle, address)
    }

    pub(super) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::connect(self, peer);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::connect(handle, peer)
    }

    pub(super) fn address(&self) -> Result<InetAddress, SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::address(self);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::address(handle)
    }

    pub(super) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::peer_address(self);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::peer_address(handle)
    }

    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
    ) -> Result<usize, SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            if target.is_some() {
                return Err(SocketError::AlreadyConnected);
            }
            return tcp::send(self, input);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::send(handle, input, target)
    }

    pub(super) fn receive(
        &self,
        output: &mut [u8],
        peek: bool,
    ) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::receive(self, output, peek);
        }
        let handle = self.udp_handle()?;
        udp_endpoint::receive(self, handle, output, peek)
    }

    pub(super) fn set_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        let handle = self.udp_handle()?;
        udp_endpoint::set_packet_info(handle, enabled);
        Ok(())
    }

    pub(super) fn packet_info(&self) -> bool {
        let Ok(handle) = self.udp_handle() else {
            return false;
        };
        udp_endpoint::packet_info(handle)
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::poll_state(self);
        }
        let Ok(handle) = self.udp_handle() else {
            return SocketPollState::error();
        };
        udp_endpoint::poll_state(handle)
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(super) fn wait_pipes(&self) -> Vec<(Arc<Pipe>, PipeDirection)> {
        vec![(self.notify_read.pipe(), PipeDirection::Read)]
    }

    /// @description 把 TCP endpoint 转换为 passive listener。
    /// @param backlog 请求的 accept queue 深度。
    /// @return listener 完整发布后返回 unit。
    /// @errors UDP、无效状态、地址或分配失败时返回错误。
    pub(super) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
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
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::connection_result(self)
        } else {
            Ok(())
        }
    }

    /// @description 读取并清除 endpoint pending error。
    /// @return TCP pending error；UDP 或无错误时为 None。
    /// @errors 无错误。
    pub(super) fn take_error(&self) -> Option<SocketError> {
        matches!(self.endpoint, InetEndpoint::Tcp(_))
            .then(|| tcp::take_error(self))
            .flatten()
    }

    /// @description 提交 TCP receive/send half-close。
    /// @param how Linux `SHUT_RD/WR/RDWR` selector。
    /// @return 成功返回 unit。
    /// @errors UDP 或未连接 TCP 返回错误。
    pub(super) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            tcp::shutdown(self, how)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }

    fn notify(&self) {
        if !self.notify_read.pipe().readable() {
            let _ = self.notify_write.write(&[1]);
        }
    }

    fn consume_notify(&self) {
        let mut byte = [0];
        if matches!(self.notify_read.read(&mut byte), PipeRead::Bytes(_)) {}
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        if let InetEndpoint::Tcp(id) = self.endpoint {
            tcp::drop_endpoint(id);
            return;
        }
        let InetEndpoint::Udp(handle) = self.endpoint else {
            unreachable!();
        };
        udp_endpoint::drop_endpoint(handle);
    }
}

/// @description standard interface ioctl 消费的不可变 Ethernet 配置快照。
#[derive(Clone, Copy)]
pub(crate) struct InterfaceSnapshot {
    pub(crate) mac: [u8; 6],
    pub(crate) address: Option<Ipv4Addr>,
    pub(crate) prefix_length: u8,
    pub(crate) up: bool,
}

/// @description procfs 消费的 interface 配置与 adapter counter 快照。
#[derive(Clone, Copy)]
pub(crate) struct NetworkSnapshot {
    pub(crate) address: Option<Ipv4Addr>,
    pub(crate) prefix_length: u8,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) up: bool,
    pub(crate) statistics: NetworkStatistics,
}

pub(crate) fn interface_snapshot() -> Result<InterfaceSnapshot, SocketError> {
    let network = stack()?.lock();
    Ok(InterfaceSnapshot {
        mac: network.device.mac_address(),
        address: network.interface_state.address,
        prefix_length: network.interface_state.prefix_length,
        up: network.interface_state.up,
    })
}

pub(crate) fn network_snapshot() -> Option<NetworkSnapshot> {
    let network = NETWORK_STACK.get()?.lock();
    Some(NetworkSnapshot {
        address: network.interface_state.address,
        prefix_length: network.interface_state.prefix_length,
        gateway: network.interface_state.gateway,
        up: network.interface_state.up,
        statistics: network.device.statistics(),
    })
}

pub(crate) fn configure_address(address: Ipv4Addr) -> Result<(), SocketError> {
    if address.is_broadcast() || address.is_multicast() || address.is_loopback() {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = stack()?.lock();
    network.interface_state.address = (!address.is_unspecified()).then_some(address);
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_netmask(mask: Ipv4Addr) -> Result<(), SocketError> {
    let bits = u32::from(mask);
    let prefix = bits.leading_ones() as u8;
    if bits != u32::MAX.checked_shl((32 - prefix) as u32).unwrap_or(0) {
        return Err(SocketError::Invalid);
    }
    let mut network = stack()?.lock();
    network.interface_state.prefix_length = prefix;
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_up(up: bool) -> Result<(), SocketError> {
    let mut network = stack()?.lock();
    network.interface_state.up = up;
    network.apply_interface_state();
    Ok(())
}

pub(crate) fn configure_gateway(gateway: Option<Ipv4Addr>) -> Result<(), SocketError> {
    if gateway.is_some_and(|address| {
        address.is_unspecified()
            || address.is_broadcast()
            || address.is_multicast()
            || address.is_loopback()
    }) {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = stack()?.lock();
    network.interface_state.gateway = gateway;
    network.apply_interface_state();
    Ok(())
}
