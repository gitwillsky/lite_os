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
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint},
};
use spin::{Mutex, Once};

use crate::{
    drivers::network::{NetworkStatistics, network_device},
    ipc::{Pipe, PipeDirection, PipeEnd, PipeRead},
    timer::get_time_us,
};

use self::device::EthernetDevice;
use super::{InetAddress, SocketError, SocketPollState};

#[path = "device.rs"]
mod device;
#[path = "inet/timing.rs"]
mod timing;
pub(crate) use timing::network_work_due;

const UDP_PACKET_SLOTS: usize = 8;
const UDP_BUFFER_BYTES: usize = 128 * 1024;
const MAX_UDP_PAYLOAD: usize = 65_507;
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
    interface_state: InterfaceState,
    next_ephemeral: u16,
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

    fn port_in_use(&self, address: Option<Ipv4Addr>, port: u16, except: SocketHandle) -> bool {
        self.endpoints.keys().any(|handle| {
            if *handle == except {
                return false;
            }
            let endpoint = self.sockets.get::<udp::Socket<'static>>(*handle).endpoint();
            endpoint.port == port
                && (endpoint.addr.is_none()
                    || address.is_none()
                    || endpoint.addr == address.map(ipv4))
        })
    }

    fn allocate_ephemeral(&mut self, handle: SocketHandle) -> Result<u16, SocketError> {
        for _ in EPHEMERAL_START..=EPHEMERAL_END {
            let candidate = self.next_ephemeral;
            self.next_ephemeral = if candidate == EPHEMERAL_END {
                EPHEMERAL_START
            } else {
                candidate + 1
            };
            if !self.port_in_use(None, candidate, handle) {
                return Ok(candidate);
            }
        }
        Err(SocketError::AddressInUse)
    }

    fn ensure_bound(&mut self, handle: SocketHandle) -> Result<(), SocketError> {
        if self.sockets.get::<udp::Socket<'static>>(handle).is_open() {
            return Ok(());
        }
        let port = self.allocate_ephemeral(handle)?;
        self.sockets
            .get_mut::<udp::Socket<'static>>(handle)
            .bind(port)
            .map_err(|_| SocketError::AddressNotAvailable)
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
            interface_state: InterfaceState {
                address: None,
                prefix_length: 0,
                gateway: None,
                up: false,
            },
            next_ephemeral: EPHEMERAL_START,
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

/// @description AF_INET UDP endpoint；协议状态和地址均保存在唯一 NetworkStack。
pub(super) struct InetSocket {
    handle: SocketHandle,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl InetSocket {
    pub(super) fn new(notify: (Arc<PipeEnd>, Arc<PipeEnd>)) -> Result<Arc<Self>, SocketError> {
        let mut rx_metadata = Vec::new();
        rx_metadata
            .try_reserve_exact(UDP_PACKET_SLOTS)
            .map_err(|_| SocketError::NoMemory)?;
        rx_metadata.resize(UDP_PACKET_SLOTS, udp::PacketMetadata::EMPTY);
        let mut tx_metadata = rx_metadata.clone();
        let mut rx_payload = Vec::new();
        rx_payload
            .try_reserve_exact(UDP_BUFFER_BYTES)
            .map_err(|_| SocketError::NoMemory)?;
        rx_payload.resize(UDP_BUFFER_BYTES, 0);
        let mut tx_payload = Vec::new();
        tx_payload
            .try_reserve_exact(UDP_BUFFER_BYTES)
            .map_err(|_| SocketError::NoMemory)?;
        tx_payload.resize(UDP_BUFFER_BYTES, 0);
        let socket = udp::Socket::new(
            udp::PacketBuffer::new(rx_metadata, rx_payload),
            udp::PacketBuffer::new(core::mem::take(&mut tx_metadata), tx_payload),
        );
        let mut network = stack()?.lock();
        let handle = network.sockets.add(socket);
        let endpoint = Arc::new(Self {
            handle,
            notify_read: notify.0,
            notify_write: notify.1,
        });
        network.endpoints.insert(
            handle,
            EndpointState {
                endpoint: Arc::downgrade(&endpoint),
                peer: None,
                packet_info: false,
            },
        );
        Ok(endpoint)
    }

    pub(super) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        let mut network = stack()?.lock();
        if network
            .sockets
            .get::<udp::Socket<'static>>(self.handle)
            .is_open()
        {
            return Err(SocketError::Invalid);
        }
        let address_filter = (!address.address.is_unspecified()).then_some(address.address);
        if address.port == 0 {
            return Err(SocketError::Invalid);
        }
        if address_filter.is_some_and(|candidate| {
            network.interface_state.address != Some(candidate) || !network.interface_state.up
        }) {
            return Err(SocketError::AddressNotAvailable);
        }
        if network.port_in_use(address_filter, address.port, self.handle) {
            return Err(SocketError::AddressInUse);
        }
        let endpoint = IpListenEndpoint {
            addr: address_filter.map(ipv4),
            port: address.port,
        };
        network
            .sockets
            .get_mut::<udp::Socket<'static>>(self.handle)
            .bind(endpoint)
            .map_err(|_| SocketError::AddressNotAvailable)
    }

    pub(super) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        if peer.port == 0 || peer.address.is_unspecified() {
            return Err(SocketError::AddressNotAvailable);
        }
        let mut network = stack()?.lock();
        network.ensure_bound(self.handle)?;
        network
            .endpoints
            .get_mut(&self.handle)
            .expect("AF_INET endpoint metadata disappeared")
            .peer = Some(peer);
        Ok(())
    }

    pub(super) fn address(&self) -> Result<InetAddress, SocketError> {
        let network = stack()?.lock();
        let endpoint = network
            .sockets
            .get::<udp::Socket<'static>>(self.handle)
            .endpoint();
        Ok(InetAddress {
            address: endpoint.addr.map(from_ip).unwrap_or(Ipv4Addr::UNSPECIFIED),
            port: endpoint.port,
        })
    }

    pub(super) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        stack()?
            .lock()
            .endpoints
            .get(&self.handle)
            .and_then(|state| state.peer)
            .ok_or(SocketError::NotConnected)
    }

    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
    ) -> Result<usize, SocketError> {
        if input.len() > MAX_UDP_PAYLOAD {
            return Err(SocketError::MessageTooLarge);
        }
        let mut network = stack()?.lock();
        if !network.interface_state.up || network.interface_state.address.is_none() {
            return Err(SocketError::NetworkUnreachable);
        }
        network.ensure_bound(self.handle)?;
        let peer = target
            .or_else(|| {
                network
                    .endpoints
                    .get(&self.handle)
                    .and_then(|state| state.peer)
            })
            .ok_or(SocketError::DestinationRequired)?;
        if peer.port == 0 || peer.address.is_unspecified() {
            return Err(SocketError::AddressNotAvailable);
        }
        network
            .sockets
            .get_mut::<udp::Socket<'static>>(self.handle)
            .send_slice(input, IpEndpoint::new(ipv4(peer.address), peer.port))
            .map_err(|error| match error {
                udp::SendError::BufferFull => SocketError::Again,
                udp::SendError::Unaddressable => SocketError::NetworkUnreachable,
            })?;
        let NetworkStack {
            interface,
            device,
            sockets,
            ..
        } = &mut *network;
        interface.poll_egress(now(), device, sockets);
        drop(network);
        Ok(input.len())
    }

    pub(super) fn receive(
        &self,
        output: &mut [u8],
        peek: bool,
    ) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
        let mut network = stack()?.lock();
        let socket = network.sockets.get_mut::<udp::Socket<'static>>(self.handle);
        let received = if peek {
            socket
                .peek()
                .map(|(payload, metadata)| (payload, *metadata))
        } else {
            socket.recv()
        };
        let result = received.map(|(payload, metadata)| {
            let full_length = payload.len();
            let count = output.len().min(full_length);
            output[..count].copy_from_slice(&payload[..count]);
            let source = InetAddress {
                address: from_ip(metadata.endpoint.addr),
                port: metadata.endpoint.port,
            };
            let local = metadata.local_address.map(from_ip);
            (count, full_length, source, local)
        });
        let drained = result.is_ok() && !peek && !socket.can_recv();
        drop(network);
        let (count, full_length, source, local) = result.map_err(|_| SocketError::Again)?;
        if drained {
            self.consume_notify();
        }
        Ok((count, full_length, source, local))
    }

    pub(super) fn set_packet_info(&self, enabled: bool) {
        stack()
            .expect("AF_INET endpoint lost network stack")
            .lock()
            .endpoints
            .get_mut(&self.handle)
            .expect("AF_INET endpoint metadata disappeared")
            .packet_info = enabled;
    }

    pub(super) fn packet_info(&self) -> bool {
        stack().is_ok_and(|stack| {
            stack
                .lock()
                .endpoints
                .get(&self.handle)
                .is_some_and(|state| state.packet_info)
        })
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        let Ok(stack) = stack() else {
            return SocketPollState::error();
        };
        let network = stack.lock();
        let socket = network.sockets.get::<udp::Socket<'static>>(self.handle);
        SocketPollState {
            readable: socket.can_recv(),
            writable: socket.can_send(),
            hangup: false,
            error: false,
        }
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(super) fn wait_pipes(&self) -> Vec<(Arc<Pipe>, PipeDirection)> {
        vec![(self.notify_read.pipe(), PipeDirection::Read)]
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
        if let Some(stack) = NETWORK_STACK.get() {
            let mut network = stack.lock();
            if network.endpoints.remove(&self.handle).is_some() {
                network.sockets.remove(self.handle);
            }
        }
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
