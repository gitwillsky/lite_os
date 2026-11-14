//! 协议服务端：把控制、域与 socket channel 的请求映射到协议引擎。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_model::network::{NetworkDevice, NetworkRssConfiguration};
use device_protocol::EventHub;
use device_protocol::directory::Class;
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Handle};
use network_protocol::{
    InetAddress, InetFamily, InetOptionChange, InetProtocol, InetReceived, InetSocketService,
    InstallerService, InterfaceSnapshot, InternetOption, InternetSendPacketInfo, Ipv4RouteError,
    Ipv4RouteRequest, NamespaceService, NetworkService, NetworkSnapshot, PacketAddress, PacketKind,
    PacketReceived, PacketSocketService, RouteChange, ServedSocket, SocketEndpoint, SocketError,
    SocketPollState,
};
use router_protocol::configuration::Name;

use crate::dispatcher::{self, Served};
use crate::inet::{self, InetSocket};
use crate::namespace::Namespace;
use crate::packet::PacketSocket;
use crate::receive::ReceiveBuffer;

/// 控制面：网络域与网卡事件分派。
pub(crate) struct Network {
    hub: Arc<EventHub>,
    forwarding: crate::worker::ForwardingWorker,
    // OWNER: 默认网络随 netstack 存续，
    // 不随任何使用者的连接退休；linuxd 重启后重新连上同一网络，接口配置不丢失。
    default: Arc<Namespace>,
    // OWNER: 稳定名称到存活域的弱索引。只在授权安装入口查询，socket/installer 寿命决定域的退休。
    managed: Mutex<fallible_tree::FallibleMap<Name, Weak<Namespace>>>,
    // OWNER: 已安装设备身份到其实际接口/寿命能力的登记；控制面重连可重新取得能力，不复制地址/FIB 或在线状态。
    // 仅安装冷路径持此锁；NET/设备事件不反向取得它。缺失登记会在配置服务单独重启后重复打开仍被 NET 独占的硬件。
    installed: Mutex<Vec<InstalledInterface>>,
}

struct InstalledInterface {
    entry: device_protocol::directory::Entry,
    scope: Weak<Namespace>,
    index: u32,
    lifetime: Channel,
    link_changed: Event,
}

/// 创建控制面与默认网络（回环启用，尚无网卡）；网卡由 router_config 经安装协议加入。
///
/// # Errors
///
/// 事件分派或默认网络创建失败。
pub(crate) fn start(forwarding: crate::worker::ForwardingWorker) -> Result<Arc<Network>, Status> {
    let default = managed_namespace()?;
    Arc::try_new(Network {
        hub: EventHub::start(Some(device_protocol::network::FORWARDING_WORKER_PRIORITY))?,
        forwarding,
        default,
        managed: Mutex::new(fallible_tree::FallibleMap::new()),
        installed: Mutex::new(Vec::new()),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)
}

fn managed_namespace() -> Result<Arc<Namespace>, Status> {
    let namespace = Namespace::create_managed().map_err(|()| native_abi::ERR_NO_MEMORY)?;
    let owner =
        inet::prepare_network(&namespace, &[], true).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    namespace.install_protocol(owner);
    let interface_events = Arc::try_new(Event::create()?).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    inet::observe_interfaces(&namespace, interface_events).map_err(installation_status)?;
    Ok(namespace)
}

/// 以执行安装服务承载原生 RIB 的一条连接。
///
/// # Errors
///
/// 服务线程创建失败。
pub(crate) fn serve_installer(network: Arc<Network>, channel: Channel) -> Result<(), Status> {
    let scope = network.default.clone();
    Installer { network, scope }.serve(channel)
}

struct Installer {
    network: Arc<Network>,
    scope: Arc<Namespace>,
}

impl Installer {
    fn serve(self, channel: Channel) -> Result<(), Status> {
        let handler = Arc::try_new(
            move |ordinal: u32,
                  request: &mut network_protocol::Reader<'_>,
                  handles: Vec<Handle>,
                  reply: &mut network_protocol::Writer| {
                network_protocol::dispatch_installer(&self, ordinal, request, handles, reply)
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        service_rpc::serve(channel, 1, b"netstack-installer", handler)
    }
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

fn installation_status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::Again => native_abi::ERR_SHOULD_WAIT,
        SocketError::NoDevice => native_abi::ERR_NOT_FOUND,
        SocketError::Invalid => native_abi::ERR_INVALID_ARGS,
        SocketError::AddressInUse => native_abi::ERR_INVALID_ARGS,
        _ => native_abi::ERR_BAD_STATE,
    }
}

impl InstallerService for Installer {
    fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status> {
        if !Arc::ptr_eq(&self.scope, &self.network.default) {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        self.network.forwarding.place(mask)
    }

    fn open_routing_domain(&self, name: Name) -> Result<(Channel, Channel), Status> {
        if !Arc::ptr_eq(&self.scope, &self.network.default) {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        let scope = {
            let mut managed = self.network.managed.lock();
            if let Some(scope) = managed.get(&name).and_then(Weak::upgrade) {
                scope
            } else {
                managed.retain(|_, scope| scope.strong_count() != 0);
                let scope = managed_namespace()?;
                managed
                    .try_insert(name, Arc::downgrade(&scope))
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                scope
            }
        };
        let namespace = serve_namespace(scope.clone())?;
        let (client, server) = Channel::create()?;
        Self {
            network: self.network.clone(),
            scope,
        }
        .serve(server)?;
        Ok((client, namespace))
    }
    fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: router_protocol::interface::InterfaceConfigurationBatch<'_>,
    ) -> Result<Vec<u8>, Status> {
        inet::project_bridges(&self.scope, ports, plans).map_err(installation_status)
    }

    fn firewall_counters(
        &self,
        cursor: router_protocol::firewall::counters::Cursor,
    ) -> Result<Vec<u8>, Status> {
        inet::firewall_counters(&self.scope, cursor)?
            .encode()
            .map_err(|_| native_abi::ERR_BAD_STATE)
    }

    fn connection_changes(
        &self,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> Result<Vec<u8>, Status> {
        inet::connection_changes(&self.scope, cursor)?
            .encode()
            .map_err(|_| native_abi::ERR_BAD_STATE)
    }

    fn arm_connection_watch(
        &self,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> Result<Option<Event>, Status> {
        let event = inet::arm_connection_watch(&self.scope, cursor, || {
            Arc::try_new(Event::create()?).map_err(|_| native_abi::ERR_NO_MEMORY)
        })?;
        event
            .map(|event| event.duplicate(rights::WAIT | rights::TRANSFER))
            .transpose()
    }

    fn clear_connection(
        &self,
        identity: router_protocol::connections::Identity,
    ) -> Result<(), Status> {
        inet::clear_connection(&self.scope, identity)
    }

    fn connections(&self, cursor: router_protocol::connections::Cursor) -> Result<Vec<u8>, Status> {
        inet::connection_page(&self.scope, cursor)?
            .encode()
            .map_err(|_| native_abi::ERR_BAD_STATE)
    }

    fn watch_routing_inputs(&self) -> Result<Event, Status> {
        inet::routing_inputs(&self.scope)
            .map_err(installation_status)?
            .duplicate(rights::SAME)
    }
    fn prepare_next_hop(
        &self,
        definition: network_protocol::routes::NextHopConfiguration<'_, core::net::Ipv4Addr>,
    ) -> Result<Channel, Status> {
        let (client, server) = Channel::create()?;
        let configuration =
            inet::prepare_next_hop(&self.scope, definition).map_err(installation_status)?;
        dispatcher::serve(server, Served::Configuration(configuration))?;
        Ok(client)
    }
    fn prepare_configuration(
        &self,
        batch: Option<network_protocol::ConfigurationBatch<'_>>,
        forwarding: Option<network_protocol::Forwarding>,
    ) -> Result<Channel, Status> {
        let (client, server) = Channel::create()?;
        let configuration = inet::prepare_configuration(&self.scope, batch, forwarding)
            .map_err(installation_status)?;
        dispatcher::serve(server, Served::Configuration(configuration))?;
        Ok(client)
    }

    fn install_device(
        &self,
        device_id: u64,
        domain: Option<Name>,
        description: Vec<u8>,
        mut handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status> {
        if !Arc::ptr_eq(&self.scope, &self.network.default) {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        if device_id == 0 || handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let scope = match domain {
            Some(name) => self
                .network
                .managed
                .lock()
                .get(&name)
                .and_then(Weak::upgrade)
                .ok_or(native_abi::ERR_BAD_STATE)?,
            None => self.scope.clone(),
        };
        let directory = device_protocol::directory::DirectoryClient::new(Channel::from_handle(
            service_rpc::take_handle(&mut handles, 0)?,
        ));
        let mut installed = self.network.installed.lock();
        // 寿命来自实际 NET 接口；关闭立即使旧登记不可复用。只在安装时退休，不添加空闲轮询或设备状态副本。
        installed.retain(|interface| {
            interface.lifetime.wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        });
        if let Some(position) = installed
            .iter()
            .position(|interface| interface.entry.id == device_id)
        {
            let interface = &installed[position];
            if interface.entry.description != description {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            if let Some(previous) = interface.scope.upgrade() {
                if Arc::ptr_eq(&previous, &scope) {
                    return Ok((
                        interface.index,
                        interface.lifetime.duplicate(rights::SAME)?,
                        interface.link_changed.duplicate(rights::SAME)?,
                    ));
                }
                // 先验证目录能力，再撤下旧域的接口和路由。目录独占会话保证新域打开的是新队列，
                // 不会收到旧域尚未处理的报文。队列未退休时打开返回可重试的 AlreadyBound。
                let (_, entries) = directory.list(device_id - 1)?;
                if !entries.iter().any(|entry| {
                    entry.id == device_id
                        && entry.class == Class::Network
                        && entry.description == description
                }) {
                    return Err(native_abi::ERR_NOT_FOUND);
                }
                inet::release_interface(&previous, interface.index).map_err(installation_status)?;
            }
            installed.remove(position);
        }
        installed
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let entry = device_protocol::directory::Entry {
            id: device_id,
            class: Class::Network,
            description,
        };
        // 接受设备已公布的自动分流队列。显式 RSS 的队列数还受可编码的间接表长度约束。
        let cpus = u16::try_from(native_runtime::system::basic().cpu_count)
            .unwrap_or(u16::MAX)
            .max(1);
        let device = device_protocol::network::connect(
            &directory,
            &entry,
            |maximum_pairs, rss, transmit| device_model::network::NetworkQueueConfiguration {
                queue_pairs: cpus.min(maximum_pairs).min(
                    rss.filter(|cap| cap.hash_types != 0)
                        .map_or(u16::MAX, |cap| 1 << cap.maximum_table_entries.ilog2()),
                ),
                transmit_bytes: transmit.maximum_frame_length,
            },
            &self.network.hub,
        )?;
        // 多队列使用同一租约内固定的表和密钥，流量入队后不因运行时 CPU 调整重新散列。
        if let Some(cap) = device.receive_scaling()
            && cap.hash_types != 0
            && device.queue_count() > 1
        {
            let count = device.queue_count();
            let entries = usize::from(count).next_power_of_two().max(128);
            if entries > usize::from(cap.maximum_table_entries) {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut indirection = Vec::new();
            indirection
                .try_reserve_exact(entries)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            for index in 0..entries {
                indirection.push((index % usize::from(count)) as u16);
            }
            let mut key = Vec::new();
            key.try_reserve_exact(40)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            key.resize(40, 0);
            native_runtime::system::cprng_draw(&mut key);
            device
                .set_receive_scaling(&NetworkRssConfiguration {
                    hash_types: cap.hash_types,
                    unclassified_queue: 0,
                    indirection,
                    key,
                })
                .map_err(device_protocol::network::encode_error)?;
        }
        let queues = crate::worker::DeviceQueues::bind(&scope, &device)?;
        let (lifetime, held) = Channel::create()?;
        let retained = lifetime.duplicate(rights::SAME)?;
        let link_changed = Event::create()?;
        let publisher = link_changed.duplicate(rights::SAME)?;
        let observer = link_changed.duplicate(rights::SAME)?;
        let index =
            inet::install_interface(&scope, device, held, publisher, queues).map_err(|error| {
                match error {
                    SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
                    _ => native_abi::ERR_NO_RESOURCES,
                }
            })?;
        // 容量与能力均已在接口安装前准备；发表后登记不再分配，回复丢失也可安全重试。
        installed.push(InstalledInterface {
            entry,
            scope: Arc::downgrade(&scope),
            index,
            lifetime: retained,
            link_changed: observer,
        });
        Ok((index, lifetime, link_changed))
    }
}

impl network_protocol::ConfigurationService for inet::ConfigurationSession {
    fn activate(&self) -> Result<(), SocketError> {
        Self::activate(self)
    }
    fn confirm(&self) -> Result<(), SocketError> {
        Self::confirm(self)
    }
    fn abort(&self) -> Result<(), SocketError> {
        Self::abort(self)
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
        serve_namespace(namespace)
    }
}

fn serve_namespace(namespace: Arc<Namespace>) -> Result<Channel, Status> {
    let (client, server) = Channel::create()?;
    dispatcher::serve(server, Served::Namespace(NamespaceServer(namespace)))?;
    Ok(client)
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
    fn socket(
        &self,
        family: network_protocol::InetFamily,
        protocol: InetProtocol,
    ) -> Result<SocketEndpoint, SocketError> {
        let readiness = readiness()?;
        let socket = InetSocket::new(family, protocol, readiness.clone(), self.0.clone())?;
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

    fn neighbor_snapshot(
        &self,
        index: u32,
    ) -> Result<network_protocol::NeighborSnapshot, SocketError> {
        inet::neighbor_snapshot(&self.0, index)
    }

    fn neighbor_page(
        &self,
        after: Option<router_protocol::neighbors::Key>,
    ) -> Result<Vec<u8>, SocketError> {
        inet::neighbor_page(&self.0, after)
    }

    fn route_page(
        &self,
        cursor: router_protocol::route_view::Cursor,
    ) -> Result<Vec<u8>, SocketError> {
        inet::route_page(&self.0, cursor)
    }

    fn address_snapshot(
        &self,
        index: u32,
    ) -> Result<network_protocol::AddressSnapshot, SocketError> {
        inet::address_snapshot(&self.0, index)
    }

    fn learned_dns(&self) -> Result<router_protocol::dns::LearnedDns, SocketError> {
        inet::learned_dns(&self.0)
    }

    fn watch_dns(&self) -> Result<Event, SocketError> {
        let event = Event::create().map_err(|_| SocketError::NoMemory)?;
        let receiver = event
            .duplicate(rights::SAME)
            .map_err(|_| SocketError::NoMemory)?;
        inet::observe_dns(&self.0, event)?;
        Ok(receiver)
    }

    fn route_snapshot(&self, family: Option<InetFamily>) -> Result<Vec<Vec<u8>>, SocketError> {
        let mut bodies = Vec::new();
        bodies
            .try_reserve_exact(if family.is_none() { 2 } else { 1 })
            .map_err(|_| SocketError::NoMemory)?;
        match family {
            Some(InetFamily::Ipv4) => {
                bodies.push(inet::route_snapshot::<core::net::Ipv4Addr>(&self.0)?.into_bytes())
            }
            Some(InetFamily::Ipv6) => {
                bodies.push(inet::route_snapshot::<core::net::Ipv6Addr>(&self.0)?.into_bytes())
            }
            None => {
                let (four, six) = inet::route_snapshots(&self.0)?;
                bodies.push(four.into_bytes());
                bodies.push(six.into_bytes());
            }
        }
        Ok(bodies)
    }

    fn route_lookup(
        &self,
        query: network_protocol::routes::RouteLookup,
    ) -> Result<network_protocol::routes::RoutePath, SocketError> {
        inet::route_lookup(&self.0, query)
    }

    fn rule_snapshot(
        &self,
        family: Option<InetFamily>,
    ) -> Result<network_protocol::routes::RuleSnapshot, SocketError> {
        inet::rule_snapshot(&self.0, family)
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
        packet_info: &[InternetSendPacketInfo],
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

    fn receive_error(
        &self,
        limit: usize,
    ) -> Result<network_protocol::InetErrorReceived, SocketError> {
        let mut output = receive_buffer(limit)?;
        let (full_length, error) = InetSocket::receive_error(self, &mut output)?;
        Ok(network_protocol::InetErrorReceived {
            data: output.into_bytes(),
            full_length,
            error,
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
            InetOptionChange::Mark(mark) => self.set_mark(mark),
            InetOptionChange::Broadcast(enabled) => self.set_broadcast(enabled),
            InetOptionChange::HopLimit(value) => self.set_hop_limit(value),
            InetOptionChange::NoDelay(enabled) => self.set_no_delay(enabled),
            InetOptionChange::Timer(option, value) => self.set_timer_option(option, value),
            InetOptionChange::MaxSegment(value) => self.set_max_segment(value),
            InetOptionChange::Ipv6PacketInfo(value) => {
                self.set_ipv6_option(InternetOption::Ipv6PacketInfo, i32::from(value))
            }
            InetOptionChange::Ipv6Only(value) => {
                self.set_ipv6_option(InternetOption::Ipv6Only, i32::from(value))
            }
            InetOptionChange::Ipv6HopLimit(value) => {
                self.set_ipv6_option(InternetOption::Ipv6HopLimit, value.map_or(-1, i32::from))
            }
            InetOptionChange::Ipv6Checksum(value) => {
                self.set_ipv6_option(InternetOption::Ipv6Checksum, value)
            }
            InetOptionChange::Ipv6Mtu(value) => {
                self.set_ipv6_option(InternetOption::Ipv6Mtu, value)
            }
            InetOptionChange::Ipv6MulticastAll(value) => {
                self.set_ipv6_option(InternetOption::Ipv6MulticastAll, i32::from(value))
            }
            InetOptionChange::ReceiveError { ipv6, enabled } => {
                self.set_receive_error(ipv6, enabled)
            }
            InetOptionChange::ReceiveErrorExtensions { ipv6, enabled } => {
                self.set_receive_error_extensions(ipv6, enabled)
            }
            InetOptionChange::AoSelection(selection) => self.select_ao_keys(selection),
            InetOptionChange::Ipv4MtuDiscovery(value) => self.set_ipv4_mtu_discovery(value),
            InetOptionChange::Ipv6DontFragment(value) => {
                self.set_ipv6_option(InternetOption::Ipv6DontFragment, i32::from(value))
            }
            InetOptionChange::Ipv4MulticastAll(value) => self.set_ipv4_multicast_all(value),
            InetOptionChange::Ipv4MulticastInterface { interface, address } => {
                self.set_ipv4_multicast_interface(interface, address)
            }
            InetOptionChange::Ipv4MulticastHopLimit(value) => self.set_ipv4_multicast_option(
                InternetOption::Ipv4MulticastHopLimit,
                value.map_or(-1, i32::from),
            ),
            InetOptionChange::Ipv4MulticastLoop(value) => {
                self.set_ipv4_multicast_option(InternetOption::Ipv4MulticastLoop, i32::from(value))
            }
            InetOptionChange::Ipv4Membership {
                group,
                interface,
                address,
                join,
            } => self.ipv4_membership(group, interface, address, join),
            InetOptionChange::Ipv6MulticastInterface(value) => {
                self.set_ipv6_option(InternetOption::Ipv6MulticastInterface, value as i32)
            }
            InetOptionChange::Ipv6MulticastHopLimit(value) => self.set_ipv6_option(
                InternetOption::Ipv6MulticastHopLimit,
                value.map_or(-1, i32::from),
            ),
            InetOptionChange::Ipv6MulticastLoop(value) => {
                self.set_ipv6_option(InternetOption::Ipv6MulticastLoop, i32::from(value))
            }
            InetOptionChange::Ipv6Membership {
                group,
                interface,
                join,
            } => self.ipv6_membership(group, interface, join),
        }
    }

    fn icmp_filter(
        &self,
        update: Option<network_protocol::IcmpFilterUpdate>,
    ) -> Result<[u32; 8], SocketError> {
        InetSocket::icmp_filter(self, update)
    }
    fn set_md5_key(
        &self,
        selector: network_protocol::TcpKeySelector,
        key: &[u8],
    ) -> Result<(), SocketError> {
        InetSocket::set_md5_key(self, selector, key)
    }

    fn set_ao_key(
        &self,
        config: network_protocol::TcpAoKey,
        key: &[u8],
        selection: network_protocol::TcpAoSelection,
    ) -> Result<(), SocketError> {
        InetSocket::set_ao_key(self, config, key, selection)
    }
    fn ao_status(&self) -> Result<network_protocol::TcpAoStatus, SocketError> {
        InetSocket::ao_status(self)
    }
    fn tcp_information(&self) -> Result<network_protocol::TcpInformation, SocketError> {
        InetSocket::tcp_information(self)
    }
    fn replace_ao_keys(
        &self,
        keys: &[(network_protocol::TcpAoKey, &[u8])],
        selection: network_protocol::TcpAoSelection,
    ) -> Result<(), SocketError> {
        InetSocket::replace_ao_keys(self, keys, selection)
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
    fn take_statistics(&self) -> Result<[u32; 2], SocketError> {
        PacketSocket::take_statistics(self)
    }

    fn set_receive_ring(
        &self,
        ring: Option<network_protocol::CaptureRing>,
    ) -> Result<(), SocketError> {
        PacketSocket::set_receive_ring(self, ring)
    }
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
        let (_, full_length, source, information) = PacketSocket::receive(self, &mut output, peek)?;
        Ok(PacketReceived {
            data: output.into_bytes(),
            full_length,
            source,
            information,
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
