//! 协议服务端：把控制、域与 socket channel 的请求映射到协议引擎。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_model::network::NetworkDevice;
use device_protocol::EventHub;
use device_protocol::directory::Class;
use native_runtime::native_abi::{self, Status, rights};
use native_runtime::{Channel, Event, Handle};
use network_protocol::{
    InetAddress, InetOptionChange, InetProtocol, InetReceived, InetSocketService, InstallerService,
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

/// 控制面：默认网络与网卡事件分派。
pub(crate) struct Network {
    hub: Arc<EventHub>,
    // OWNER: 默认网络（对应 Fuchsia netstack 的默认栈）：持有 netcfg 安装的全部网卡，随 netstack 存续，
    // 不随任何使用者的连接退休；linuxd 重启后重新连上同一网络，接口配置不丢失。
    default: Arc<Namespace>,
}

/// 创建控制面与默认网络（回环启用，尚无网卡）；网卡由 netcfg 经安装协议加入。
///
/// # Errors
///
/// 事件分派或默认网络创建失败。
pub(crate) fn start() -> Result<Arc<Network>, Status> {
    let namespace = Namespace::create().map_err(|()| native_abi::ERR_NO_MEMORY)?;
    let owner =
        inet::prepare_network(&namespace, &[], true).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    namespace.install_protocol(owner);
    Arc::try_new(Network {
        hub: EventHub::start()?,
        default: namespace,
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

/// 以安装服务服务 netcfg 的一条连接。
///
/// # Errors
///
/// 服务线程创建失败。
pub(crate) fn serve_installer(network: Arc<Network>, channel: Channel) -> Result<(), Status> {
    let handler = Arc::try_new(
        move |ordinal: u32,
              request: &mut network_protocol::Reader<'_>,
              handles: Vec<Handle>,
              reply: &mut network_protocol::Writer| {
            network_protocol::dispatch_installer(&*network, ordinal, request, handles, reply)
        },
    )
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    service_rpc::serve(channel, 1, b"netstack-installer", handler)
}

/// 以控制服务服务一条使用者连接；全部连接共享同一控制面。
///
/// # Errors
///
/// 服务线程创建失败。
pub(crate) fn serve_control(network: Arc<Network>, control: Channel) -> Result<(), Status> {
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

impl InstallerService for Network {
    fn install_device(
        &self,
        device_id: u64,
        description: Vec<u8>,
        mut handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status> {
        let directory = device_protocol::directory::DirectoryClient::new(Channel::from_handle(
            service_rpc::take_handle(&mut handles, 0)?,
        ));
        // 当前协议引擎由单一 worker 推进；队列策略属于使用者，不能由驱动按 CPU 数猜测。
        let device = device_protocol::network::connect(
            &directory,
            &device_protocol::directory::Entry {
                id: device_id,
                class: Class::Network,
                description,
            },
            1,
            &self.hub,
        )?;
        let activity = crate::worker::device_activity()?;
        for queue in 0..device.queue_count() {
            device.bind_activity(queue, activity.clone());
        }
        let (lifetime, held) = Channel::create()?;
        let index =
            inet::install_interface(&self.default, device, held).map_err(|error| match error {
                SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
                _ => native_abi::ERR_NO_RESOURCES,
            })?;
        Ok((index, lifetime))
    }
}

impl NetworkService for Network {
    fn create_namespace(&self, initial: bool) -> Result<Channel, Status> {
        let namespace = if initial {
            self.default.clone()
        } else {
            // 新网络域与 Linux NEWNET 一致：只有 DOWN 且无地址的回环，没有网卡。
            let namespace = Namespace::create().map_err(|()| native_abi::ERR_NO_MEMORY)?;
            let owner = inet::prepare_network(&namespace, &[], false)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            namespace.install_protocol(owner);
            namespace
        };
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
