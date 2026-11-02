//! 协议服务端：把控制、域与 socket channel 的请求映射到协议引擎。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_model::network::NetworkDevice;
use device_protocol::EventHub;
use device_protocol::directory::{Announcement, Class};
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::sync::Once;
use native_runtime::{Channel, Event, Handle};
use network_protocol::{
    InetAddress, InetOptionChange, InetProtocol, InetReceived, InetSocketService,
    InterfaceSnapshot, InternetOption, InternetSendPacketInfo, Ipv4RouteError, Ipv4RouteRequest,
    NamespaceService, NetworkService, NetworkSnapshot, PacketAddress, PacketKind, PacketReceived,
    PacketSocketService, RouteChange, RouteSnapshot, ServedSocket, SocketEndpoint, SocketError,
    SocketPollState,
};

use crate::dispatcher::{self, Served};
use crate::inet::{self, InetSocket};
use crate::namespace::Namespace;
use crate::packet::PacketSocket;
use crate::receive::ReceiveBuffer;
use crate::sync::Mutex;

/// 控制面：已接入的网卡与它们的事件分派。
struct Network {
    hub: Arc<EventHub>,
    // OWNER: 已接入网卡的唯一列表（接入顺序）；初始域按它装配以太网接口。
    devices: Mutex<Vec<Arc<dyn NetworkDevice>>>,
    // OWNER: 初始域只创建一次；它独占全部网卡，其他域只有回环。
    initial: Once<()>,
}

/// 以控制服务服务 `control`。
///
/// # Errors
///
/// 事件分派或服务线程创建失败。
pub(crate) fn serve_control(control: Channel) -> Result<(), Status> {
    let network = Arc::try_new(Network {
        hub: EventHub::start()?,
        devices: Mutex::new(Vec::new()),
        initial: Once::new(),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let handler = Arc::try_new(
        move |ordinal: u32,
              request: &mut network_protocol::Reader<'_>,
              handles: Vec<Handle>,
              reply: &mut network_protocol::Writer| {
            network_protocol::dispatch_control(&*network, ordinal, request, handles, reply)
        },
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    service_rpc::serve(control, 1, b"netstack-control", handler)
}

impl NetworkService for Network {
    fn attach_device(&self, description: Vec<u8>, handles: Vec<Handle>) -> Result<(), Status> {
        let device = device_protocol::network::connect(
            Announcement {
                class: Class::Network,
                description,
                handles,
            },
            &self.hub,
        )?;
        device.bind_activity(crate::worker::device_activity()?);
        let mut devices = self.devices.lock();
        devices
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        devices.push(device);
        Ok(())
    }

    fn create_namespace(&self, initial: bool) -> Result<Channel, Status> {
        if initial && self.initial.get().is_some() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let namespace = Namespace::create().map_err(|()| native_abi::ERR_NO_MEMORY)?;
        let devices = if initial {
            self.devices.lock().clone()
        } else {
            Vec::new()
        };
        let owner = inet::prepare_network(&namespace, &devices, initial)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        namespace.install_protocol(owner);
        if initial {
            self.initial.call_once(|| ());
        }
        let (client, server) = Channel::create()?;
        dispatcher::serve(server, Served::Namespace(NamespaceServer(namespace)))?;
        Ok(client)
    }
}

/// 为新 socket 建立 channel 与就绪 Event，并登记到分派器。
pub(crate) fn publish(
    socket: ServedSocket,
    readiness: &Event,
) -> Result<SocketEndpoint, SocketError> {
    let readiness = readiness
        .duplicate(rights::SAME)
        .map_err(|_| SocketError::NoMemory)?;
    let (client, server) = Channel::create().map_err(|_| SocketError::NoMemory)?;
    dispatcher::serve(server, Served::Socket(socket)).map_err(|_| SocketError::NoMemory)?;
    Ok(SocketEndpoint {
        channel: client,
        readiness,
    })
}

/// 新 socket 的就绪 Event：服务端置位 `USER_0`，使用者等待并清除它的副本。
pub(crate) fn readiness() -> Result<Arc<Event>, SocketError> {
    Arc::try_new(Event::create().map_err(|_| SocketError::NoMemory)?)
        .map_err(|_| SocketError::NoMemory)
}

/// 域 channel 服务的对象：新 socket 各持该域的一个引用。
pub(crate) struct NamespaceServer(pub(crate) Arc<Namespace>);

impl NamespaceService for NamespaceServer {
    fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError> {
        let readiness = readiness()?;
        let socket = InetSocket::new(protocol, readiness.clone(), self.0.clone())?;
        publish(ServedSocket::Inet(socket), &readiness)
    }

    fn packet_socket(
        &self,
        kind: PacketKind,
        protocol: u16,
    ) -> Result<SocketEndpoint, SocketError> {
        let readiness = readiness()?;
        let socket = PacketSocket::new(kind, protocol, readiness.clone(), self.0.clone())?;
        publish(ServedSocket::Packet(socket), &readiness)
    }

    fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError> {
        inet::interface_index(&self.0, name)
    }

    fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError> {
        inet::interface_snapshot(&self.0, index)
    }

    fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError> {
        inet::interface_snapshots(&self.0)
    }

    fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError> {
        inet::network_snapshot(&self.0)
    }

    fn route_snapshot(&self) -> Result<Vec<RouteSnapshot>, SocketError> {
        inet::route_snapshot(&self.0)
    }

    fn configure_mtu(&self, index: u32, mtu: i32) -> Result<(), SocketError> {
        inet::configure_mtu(&self.0, index, mtu)
    }

    fn configure_address(&self, index: u32, address: Ipv4Addr) -> Result<(), SocketError> {
        inet::configure_address(&self.0, index, address)
    }

    fn configure_netmask(&self, index: u32, netmask: Ipv4Addr) -> Result<(), SocketError> {
        inet::configure_netmask(&self.0, index, netmask)
    }

    fn configure_broadcast(&self, index: u32, broadcast: Ipv4Addr) -> Result<(), SocketError> {
        inet::configure_broadcast(&self.0, index, broadcast)
    }

    fn configure_up(&self, index: u32, up: bool) -> Result<(), SocketError> {
        inet::configure_up(&self.0, index, up)
    }

    fn configure_route(
        &self,
        request: Ipv4RouteRequest,
        change: RouteChange,
    ) -> Result<(), Ipv4RouteError> {
        inet::configure_route(&self.0, request, change)
    }
}

fn receive_buffer(limit: usize) -> Result<ReceiveBuffer, SocketError> {
    ReceiveBuffer::try_new(limit).map_err(|()| SocketError::NoMemory)
}

impl InetSocketService for InetSocket {
    fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        InetSocket::bind(self, address)
    }

    fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        InetSocket::connect(self, peer)
    }

    fn disconnect(&self) -> Result<(), SocketError> {
        InetSocket::disconnect(self)
    }

    fn address(&self) -> Result<InetAddress, SocketError> {
        InetSocket::address(self)
    }

    fn peer_address(&self) -> Result<InetAddress, SocketError> {
        InetSocket::peer_address(self)
    }

    fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: Option<InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        InetSocket::send_to(self, input, target, packet_info)
    }

    fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError> {
        let mut output = receive_buffer(limit)?;
        let (_, full_length, source, packet_info) = InetSocket::receive(self, &mut output, peek)?;
        Ok(InetReceived {
            data: output.into_bytes(),
            full_length,
            source,
            packet_info,
        })
    }

    fn is_listening(&self) -> Result<bool, SocketError> {
        InetSocket::is_listening(self)
    }

    fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        InetSocket::listen(self, backlog)
    }

    fn accept(&self) -> Result<SocketEndpoint, SocketError> {
        let readiness = readiness()?;
        let accepted = InetSocket::accept(self, readiness.clone())?;
        publish(ServedSocket::Inet(accepted), &readiness)
    }

    fn connection_result(&self) -> Result<(), SocketError> {
        InetSocket::connection_result(self)
    }

    fn take_error(&self) -> Option<SocketError> {
        InetSocket::take_error(self)
    }

    fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        InetSocket::shutdown(self, how)
    }

    fn poll_state(&self) -> SocketPollState {
        InetSocket::poll_state(self)
    }

    fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError> {
        match change {
            InetOptionChange::HeaderIncluded(enabled) => self.set_header_included(enabled),
            InetOptionChange::PacketInfo(enabled) => self.set_packet_info(enabled),
            InetOptionChange::ReuseAddress(enabled) => self.set_reuse_address(enabled),
            InetOptionChange::Broadcast(enabled) => self.set_broadcast(enabled),
            InetOptionChange::HopLimit(value) => self.set_hop_limit(value),
            InetOptionChange::NoDelay(enabled) => self.set_no_delay(enabled),
            InetOptionChange::Timer(option, value) => self.set_timer_option(option, value),
            InetOptionChange::MaxSegment(value) => self.set_max_segment(value),
        }
    }

    fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError> {
        InetSocket::bind_to_device(self, name, privileged)
    }

    fn option(&self, option: InternetOption) -> Result<i32, SocketError> {
        InetSocket::option(self, option)
    }

    fn close(&self) {
        self.close_description();
    }
}

impl PacketSocketService for PacketSocket {
    fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        PacketSocket::bind(self, address)
    }

    fn address(&self) -> Result<PacketAddress, SocketError> {
        PacketSocket::address(self)
    }

    fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError> {
        PacketSocket::send_to(self, input, target)
    }

    fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError> {
        let mut output = receive_buffer(limit)?;
        let (_, full_length, source) = PacketSocket::receive(self, &mut output, peek)?;
        Ok(PacketReceived {
            data: output.into_bytes(),
            full_length,
            source,
        })
    }

    fn take_error(&self) -> Option<SocketError> {
        PacketSocket::take_error(self)
    }

    fn poll_state(&self) -> SocketPollState {
        PacketSocket::poll_state(self)
    }

    fn close(&self) {
        self.close_description();
    }
}
