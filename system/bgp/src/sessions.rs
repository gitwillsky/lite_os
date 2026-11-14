//! 单 actor 的原生邻居运行；会话、流字节和域路由各沿唯一 owner，配置能力关闭即停止。
mod observation;

use crate::connection::{
    Connection, Io, PeerScope, Transport, WORK_BUDGET, export_error, install_ao, next_key,
    route_error, status, time,
};
use ::bgp::{
    Config, Input, Notice, OpenOffer, RoutingDomain, SelectionContext, State, prefer_outgoing,
};
use alloc::{format, string::String, vec::Vec};
use core::{net::Ipv6Addr, num::NonZeroU32};
use native_runtime::{
    Channel, Port, Vmo,
    native_abi::{self, Status, signals},
};
use network_protocol::{InetAddress, InetProtocol, RemoteNamespace, SocketError, TcpKeySelector};
use router_protocol::{
    bgp::{Configuration, Neighbor},
    configuration::Name,
};

const CONTROL: u64 = 0;
const NETWORK: u64 = 1;
const LISTENER: u64 = 2;
const RIB: u64 = 3;
const ROUTES: u64 = 4;
// 整体来源按 50 ms 合并接收变化，避免每份 UPDATE 重编译全表；真实待办由 routes.pending 持有。
const ROUTE_BATCH_NS: u64 = 50_000_000;

struct NeighborState {
    name: Name,
    parameters: Neighbor,
    scope: PeerScope,
    outgoing: Connection,
    incoming: Option<Connection>,
    // OWNER: 只保留最后一个实际连接错误供观察；Session 只拥有协议状态/截止，不拥有历史诊断。
    error: Option<String>,
    // OWNER: 下一条主动/被动 TCP 共享此协商决定；None 表示能力不相容，等待配置重建。
    // 不能放入短寿命 incoming Connection，否则每次入站都遗忘对端拒绝并重复原 OPEN。
    open_offer: Option<OpenOffer>,
    // A durable desired key set may have reached only some sockets. Replay the complete
    // replacement on retry even if the newest desired parameters are identical.
    ao_pending: bool,
}
impl NeighborState {
    fn stop(
        &mut self,
        network: &RemoteNamespace,
        port: &Port,
        key: &mut u64,
        routes: &mut RoutingDomain,
    ) -> Result<(), Status> {
        let mut io = Io {
            network,
            port,
            key,
            routes,
            parameters: &self.parameters,
            scope: self.scope,
            error: &mut self.error,
            open_offer: &mut self.open_offer,
        };
        let outgoing = self.outgoing.stop(&mut io);
        if let Some(incoming) = &mut self.incoming {
            incoming.stop(&mut io)?;
            self.incoming = None;
        }
        outgoing
    }
}
pub(super) struct Sessions {
    control: Channel,
    network: RemoteNamespace,
    port: Port,
    listener: Transport,
    peers: Vec<NeighborState>,
    routes: RoutingDomain,
    key: u64,
    source: network_protocol::routing::RoutingSource,
    changes: network_protocol::routing::Tracking,
    minimum: NonZeroU32,
    // OWNER: 有待处理来源工作时的唯一截止；没有工作不轮询，不保存另一份选路状态。
    routes_at: Option<u64>,
}
impl Sessions {
    pub(super) fn new(
        settings: network_protocol::bgp::SessionConfiguration,
        control: Channel,
        network: RemoteNamespace,
        source: network_protocol::routing::RoutingSource,
        minimum: NonZeroU32,
    ) -> Result<Self, Status> {
        let network_protocol::bgp::SessionConfiguration {
            configuration,
            scopes,
        } = settings;
        configuration
            .validate()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let port = Port::create()?;
        let changes = source.watch_routes()?;
        source
            .channel()
            .wait_async(&port, RIB, signals::PEER_CLOSED)?;
        changes
            .channel()
            .wait_async(&port, RIB, signals::PEER_CLOSED)?;
        changes
            .changes()
            .wait_async(&port, ROUTES, signals::USER_0)?;
        network
            .channel()
            .wait_async(&port, NETWORK, signals::PEER_CLOSED)?;
        control.wait_async(&port, CONTROL, signals::READABLE | signals::PEER_CLOSED)?;
        let listener = Transport::new(
            network
                .socket(network_protocol::InetFamily::Ipv6, InetProtocol::Tcp)
                .map_err(status)?,
            LISTENER,
        )?;
        listener
            .socket
            .set_option(network_protocol::InetOptionChange::ReuseAddress(true))
            .map_err(status)?;
        listener
            .socket
            .bind(InetAddress::new(Ipv6Addr::UNSPECIFIED.into(), 179))
            .map_err(status)?;
        // 所有邻居的策略必须在 listen 之前安装，不能先接受未受保护的 SYN。
        let peer_scope = |position| {
            scopes.as_ref().map_or(PeerScope::InterfaceName, |rows| {
                PeerScope::Installed(rows[position])
            })
        };
        for (position, neighbor) in configuration.neighbors.values().enumerate() {
            if neighbor.md5_password.is_some() || neighbor.tcp_ao.is_some() {
                let scope = peer_scope(position);
                if scope.unavailable(neighbor) {
                    continue;
                }
                let scope = scope.index(&network, neighbor).map_err(status)?;
                let selector = TcpKeySelector::host(neighbor.address, scope);
                if let Some(ao) = &neighbor.tcp_ao {
                    install_ao(&listener.socket, selector, ao, true).map_err(status)?;
                } else if let Some(password) = &neighbor.md5_password {
                    listener
                        .socket
                        .set_md5_key(selector, password.as_bytes())
                        .map_err(status)?;
                }
            }
        }
        listener
            .socket
            .listen(configuration.neighbors.len().max(1))
            .map_err(status)?;
        let context = SelectionContext::new(
            configuration.local_as.get(),
            configuration.identifier,
            configuration.reflection_cluster(),
            configuration
                .confederation
                .as_ref()
                .map(|confederation| confederation.identifier.get()),
        )
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut retain_paths = [false; 2];
        for neighbor in configuration.neighbors.values() {
            if neighbor.send_add_path && neighbor.export.is_some() {
                retain_paths[0] |= neighbor.ipv4_unicast;
                retain_paths[1] |= neighbor.ipv6_unicast;
            }
        }
        let mut routes =
            RoutingDomain::try_new(context, retain_paths).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if configuration
            .neighbors
            .values()
            .any(|neighbor| neighbor.export.is_some())
        {
            Self::observe_network(&mut routes, &network)?;
        }
        let queries = routes
            .configure_networks(configuration.networks)
            .map_err(route_error)?;
        if !queries.is_empty() {
            changes.update(&queries)?;
        }
        routes
            .configure_redistribution(configuration.redistribute)
            .map_err(route_error)?;
        let routes_at =
            (routes.has_networks() || routes.redistribution_request().is_some()).then(time);
        let mut peers = Vec::new();
        peers
            .try_reserve_exact(configuration.neighbors.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (position, (name, parameters)) in configuration.neighbors.into_iter().enumerate() {
            let settings = Config {
                local_as: configuration.confederation.as_ref().map_or_else(
                    || configuration.local_as.get(),
                    |confederation| {
                        confederation.session_as(configuration.local_as, parameters.asn)
                    },
                ),
                confederation_peer: configuration.confederation.as_ref().map(|confederation| {
                    confederation.peer_kind(configuration.local_as, parameters.asn)
                }),
                peer_as: parameters.asn.get(),
                identifier: configuration.identifier,
                hold_seconds: parameters.hold_seconds,
                retry_seconds: parameters.retry_seconds.get(),
                passive: parameters.passive,
                ipv4_unicast: parameters.ipv4_unicast,
                ipv6_unicast: parameters.ipv6_unicast,
                receive_add_path: parameters.receive_add_path,
                send_add_path: parameters.send_add_path,
                route_refresh: true,
                extended_messages: false,
            };
            peers.push(NeighborState {
                name,
                outgoing: Connection::new(settings)?,
                incoming: None,
                scope: peer_scope(position),
                error: None,
                parameters,
                open_offer: Some(OpenOffer::Capabilities),
                ao_pending: false,
            });
        }
        Ok(Self {
            control,
            network,
            port,
            listener,
            peers,
            routes,
            key: 5,
            source,
            changes,
            minimum,
            routes_at,
        })
    }
    fn update_neighbors(
        peers: &mut [NeighborState],
        listener: &Transport,
        network: &RemoteNamespace,
        port: &Port,
        key: &mut u64,
        routes: &mut RoutingDomain,
        mut configuration: Configuration,
    ) -> Result<(), Status> {
        if configuration.neighbors.len() != peers.len()
            || peers.iter().any(|peer| {
                configuration
                    .neighbors
                    .get(&peer.name)
                    .is_none_or(|next| !peer.parameters.supports_live_update(next))
            })
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut failure = None;
        for peer in peers {
            let mut next = configuration
                .neighbors
                .remove(&peer.name)
                .expect("validated peer");
            let starting = peer.parameters.shutdown && !next.shutdown;
            if next.shutdown || starting {
                // 关闭失败时先封住主动重连和入站接纳，原配置重试仍持有尚待退休的连接。
                peer.parameters.shutdown = true;
                if let Err(error) = peer.stop(network, port, key, routes) {
                    failure.get_or_insert(error);
                    continue;
                }
            }
            if peer.parameters.tcp_ao != next.tcp_ao {
                peer.parameters.tcp_ao = next.tcp_ao.take();
                peer.ao_pending = true;
            }
            if peer.ao_pending {
                let result = (|| {
                    if peer.scope.unavailable(&peer.parameters) {
                        return Ok(());
                    }
                    let scope = peer.scope.index(network, &peer.parameters)?;
                    let selector = TcpKeySelector::host(peer.parameters.address, scope);
                    let ao = peer
                        .parameters
                        .tcp_ao
                        .as_ref()
                        .expect("pending AO update has keys");
                    install_ao(&listener.socket, selector, ao, true)?;
                    for connection in core::iter::once(&peer.outgoing).chain(peer.incoming.iter()) {
                        if let Some(transport) = &connection.transport {
                            install_ao(&transport.socket, selector, ao, false)?;
                        }
                    }
                    Ok(())
                })();
                match result {
                    Ok(()) => peer.ao_pending = false,
                    Err(error) => {
                        failure.get_or_insert_with(|| status(error));
                        continue;
                    }
                }
            }
            if starting {
                peer.parameters.shutdown = false;
                peer.open_offer = Some(OpenOffer::Capabilities);
                peer.error = None;
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn control(&mut self) -> Result<bool, Status> {
        loop {
            match self.control.read() {
                Ok(message) => match service_rpc::respond(
                    &self.control,
                    message,
                    |ordinal, reader, handles, reply| {
                        use network_protocol::bgp::{REFRESH, ROUTES, SNAPSHOT, UPDATE_NEIGHBORS};
                        if reader.u32()? != router_protocol::bgp::VERSION {
                            return Err(native_abi::ERR_INVALID_ARGS);
                        }
                        let pending = self.routes_at.is_some() || !self.routes.pending().is_empty();
                        let bytes = match ordinal {
                            SNAPSHOT => {
                                reader.finish()?;
                                if !handles.is_empty() {
                                    return Err(native_abi::ERR_INVALID_ARGS);
                                }
                                observation::snapshot(&self.peers, &self.routes, pending)?
                                    .encode()
                                    .map_err(|_| native_abi::ERR_BAD_STATE)?
                            }
                            ROUTES | REFRESH | UPDATE_NEIGHBORS => {
                                let length = reader.u64()?;
                                reader.finish()?;
                                if handles.len() != 1 {
                                    return Err(native_abi::ERR_INVALID_ARGS);
                                }
                                let mut handles = handles;
                                let content =
                                    Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?)
                                        .read_bytes(length)?;
                                if ordinal == UPDATE_NEIGHBORS {
                                    let configuration = Configuration::parse(&content)
                                        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                                    Self::update_neighbors(
                                        &mut self.peers,
                                        &self.listener,
                                        &self.network,
                                        &self.port,
                                        &mut self.key,
                                        &mut self.routes,
                                        configuration,
                                    )?;
                                    return Ok(Vec::new());
                                }
                                if ordinal == REFRESH {
                                    let request =
                                        router_protocol::bgp::RefreshRequest::parse(&content)
                                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                                    let peer = self
                                        .peers
                                        .iter_mut()
                                        .find(|peer| peer.name == request.neighbor)
                                        .ok_or(native_abi::ERR_NOT_FOUND)?;
                                    let connection = core::iter::once(&mut peer.outgoing)
                                        .chain(peer.incoming.iter_mut())
                                        .find(|connection| {
                                            !peer.parameters.shutdown
                                                && connection.session.state() == State::Established
                                        })
                                        .ok_or(native_abi::ERR_SHOULD_WAIT)?;
                                    let family = match request.family {
                                        router_protocol::routing::Family::Ipv4 => {
                                            packet_wire::bgp::nlri::Family::Ipv4
                                        }
                                        router_protocol::routing::Family::Ipv6 => {
                                            packet_wire::bgp::nlri::Family::Ipv6
                                        }
                                    };
                                    connection.request_refresh(family)?;
                                    return Ok(Vec::new());
                                }
                                let query = router_protocol::tracking::IpPrefix::parse(&content)
                                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                                observation::observe_routes(
                                    &self.peers,
                                    &self.routes,
                                    pending,
                                    query,
                                )?
                                .encode()
                                .map_err(|_| native_abi::ERR_BAD_STATE)?
                            }
                            _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                        };
                        let content = Vmo::from_bytes(&bytes)?;
                        let mut returned = Vec::new();
                        returned
                            .try_reserve_exact(1)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        reply.try_reserve(8)?;
                        reply.u64(bytes.len() as u64);
                        returned.push(content.into_handle());
                        Ok(returned)
                    },
                ) {
                    Ok(()) => {}
                    // 查询在途时配置能力也可能关闭，仍应走全域行政清理，而非绕过 Stop。
                    Err(native_abi::ERR_PEER_CLOSED) => return Ok(false),
                    Err(error) => return Err(error),
                },
                Err(native_abi::ERR_SHOULD_WAIT) => return Ok(true),
                Err(native_abi::ERR_PEER_CLOSED) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
    }
    fn accept(&mut self) -> Result<bool, Status> {
        // 未配置地址也能发起 TCP；限制每轮 accept，避免外部连接洪泛饿死 hold timer/控制请求。
        for _ in 0..WORK_BUDGET {
            let endpoint = match self.listener.socket.accept() {
                Ok(endpoint) => endpoint,
                Err(SocketError::Again) => return Ok(false),
                Err(error) => return Err(status(error)),
            };
            let candidate = Transport::new(endpoint, next_key(&mut self.key)?)?;
            let endpoint = candidate.socket.peer_address().map_err(status)?;
            let address = endpoint.ip().to_canonical();
            let scope = match endpoint {
                InetAddress::V6(address) if address.ip().is_unicast_link_local() => {
                    Some(address.scope_id())
                }
                _ => None,
            };
            // scope 来自实际 TCP；每条入站只查询一次接口名，不能为同地址的每个配置邻居重复发 RPC。
            let interface = scope.and_then(|index| self.network.interface_snapshot(index).ok());
            let interface_name = interface.as_ref().map(|interface| {
                &interface.name[..interface
                    .name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(interface.name.len())]
            });
            let Some(peer) = self.peers.iter_mut().find(|peer| {
                !peer.parameters.shutdown
                    && peer.parameters.address == address
                    && match peer.scope {
                        PeerScope::InterfaceName => {
                            peer.parameters
                                .interface
                                .as_ref()
                                .map(|name| name.as_str().as_bytes())
                                == interface_name
                        }
                        PeerScope::Installed(index) => {
                            !peer.scope.unavailable(&peer.parameters)
                                && scope == (index != 0).then_some(index)
                        }
                    }
                    && peer.incoming.is_none()
                    && peer.open_offer.is_some()
            }) else {
                candidate.notify(
                    Notice {
                        code: 6,
                        subcode: 5,
                        data: &[],
                    },
                    false,
                );
                continue;
            };
            if let Some(ao) = &peer.parameters.tcp_ao
                && let Err(error) = install_ao(
                    &candidate.socket,
                    TcpKeySelector::host(address, scope),
                    ao,
                    false,
                )
            {
                // SYN may have been accepted before the listener's latest key change.
                // Establish the desired policy before this stream can carry a BGP OPEN.
                peer.error = Some(format!("TCP-AO: {error:?}"));
                continue;
            }
            let offer = peer.open_offer.expect("eligible peer can negotiate");
            let mut settings = peer.outgoing.session.config();
            settings.passive = true;
            let mut connection = Connection::new(settings)?;
            connection.transport = Some(candidate);
            let mut io = Io {
                network: &self.network,
                port: &self.port,
                key: &mut self.key,
                routes: &mut self.routes,
                parameters: &peer.parameters,
                scope: peer.scope,
                error: &mut peer.error,
                open_offer: &mut peer.open_offer,
            };
            connection.event(Input::Start, &mut io)?;
            connection.event(Input::Connected(offer), &mut io)?;
            peer.incoming = Some(connection);
        }
        Ok(true)
    }
    fn observe_network(
        routes: &mut RoutingDomain,
        network: &RemoteNamespace,
    ) -> Result<bool, Status> {
        let addresses = network.address_snapshot(0).map_err(status)?;
        routes
            .observe_export_network((0..addresses.len()).filter_map(|index| {
                let row = addresses.get(index)?;
                let core::net::IpAddr::V6(address) = row.address else {
                    return None;
                };
                row.usable.then_some(::bgp::ExportAddress {
                    interface: NonZeroU32::new(row.interface_index)?,
                    address,
                    prefix_length: row.prefix_length,
                    on_link: row.on_link,
                })
            }))
            .map_err(route_error)
    }
    fn apply_routes(&mut self) -> Result<(), Status> {
        // 1. 接口/DAD 变化与选路同走原图通知；捕获成功后再接管，不能按旧地址资格继续发布。
        if self
            .peers
            .iter()
            .any(|peer| peer.parameters.export.is_some())
            && Self::observe_network(&mut self.routes, &self.network)?
        {
            // 2. 只登记有界游标，不在接口事件中遍历全表；包含先前因范围而不能发布的目标。
            for peer in &mut self.peers {
                if peer.parameters.shutdown || peer.parameters.export.is_none() {
                    continue;
                }
                for connection in
                    core::iter::once(&mut peer.outgoing).chain(peer.incoming.iter_mut())
                {
                    if let Some((context, output)) = &mut connection.output
                        && (context.ipv6_unicast || context.send_ipv4_via_ipv6)
                    {
                        if context.ipv6_unicast {
                            output.reconsider(packet_wire::bgp::nlri::Family::Ipv6);
                        }
                        if context.send_ipv4_via_ipv6 {
                            output.reconsider(packet_wire::bgp::nlri::Family::Ipv4);
                        }
                        if let Some(transport) = &mut connection.transport {
                            transport.schedule(&self.port)?;
                        }
                    }
                }
            }
        }
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(self.peers.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for peer in &self.peers {
            for connection in core::iter::once(&peer.outgoing).chain(peer.incoming.iter()) {
                if connection.session.state() == State::Established
                    && let Some(transport) = &connection.transport
                {
                    bindings.push((transport.key, &peer.parameters));
                }
            }
        }
        bindings.sort_unstable_by_key(|(id, _)| *id);
        let changed = match crate::route_selection::reconcile(
            &mut self.routes,
            &self.source,
            self.minimum,
            &bindings,
        ) {
            Ok(changed) => changed,
            Err(native_abi::ERR_SHOULD_WAIT) => {
                self.routes_at = Some(time().saturating_add(ROUTE_BATCH_NS));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        self.routes_at = None;
        // 先登记已来源确认的学习变化；随后本机证明暂不可读时不能丢失这些出口义务。
        Self::queue_exports(&mut self.peers, &self.port, &self.routes, || {
            changed.iter().copied()
        })?;
        if self.routes.has_networks() {
            match self.changes.snapshot() {
                Ok(snapshot) => {
                    let changed = self
                        .routes
                        .update_networks(snapshot.view())
                        .map_err(route_error)?;
                    Self::queue_exports(&mut self.peers, &self.port, &self.routes, || {
                        changed.iter().copied()
                    })?;
                }
                // 首个 RIB 图尚未发表时等已有 ROUTES Event；不为未就绪的网段启动周期轮询。
                Err(native_abi::ERR_SHOULD_WAIT) => {}
                Err(error) => return Err(error),
            }
        }
        if let Some((selector, generation)) = self.routes.redistribution_request() {
            match self.changes.route_changes(selector, generation) {
                Ok(changes) => {
                    let changed = self
                        .routes
                        .apply_redistribution(&changes)
                        .map_err(route_error)?;
                    Self::queue_exports(&mut self.peers, &self.port, &self.routes, || {
                        changed.iter().copied()
                    })?;
                }
                Err(native_abi::ERR_SHOULD_WAIT) => {} // 等原已确认图 Event，不增加周期轮询。
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn queue_exports<I: Iterator<Item = packet_wire::bgp::nlri::Prefix>>(
        peers: &mut [NeighborState],
        port: &Port,
        routes: &RoutingDomain,
        changed: impl Fn() -> I,
    ) -> Result<(), Status> {
        let now = time();
        for peer in peers {
            if peer.parameters.shutdown || peer.parameters.export.is_none() {
                continue;
            }
            for connection in core::iter::once(&mut peer.outgoing).chain(peer.incoming.iter_mut()) {
                if let Some((context, output)) = &mut connection.output {
                    let mut changed = changed().peekable();
                    if changed.peek().is_none() {
                        continue;
                    }
                    for prefix in changed {
                        if if prefix.address.is_ipv4() {
                            !context.ipv4_unicast
                        } else {
                            !context.ipv6_unicast
                        } {
                            continue;
                        }
                        output.changed(routes, prefix, now).map_err(export_error)?;
                    }
                    if let Some(transport) = &mut connection.transport {
                        transport.schedule(port)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn stop(mut self) -> Result<(), Status> {
        let mut failure = None;
        for peer in &mut self.peers {
            // 所有连接都尝试发送行政关闭通知；一个连接清理失败不能阻止其他 TCP 和路由域最终释放。
            if let Err(error) =
                peer.stop(&self.network, &self.port, &mut self.key, &mut self.routes)
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub(super) fn run(mut self) -> Result<(), Status> {
        let mut wake = None;
        loop {
            if !self.control()? {
                return self.stop();
            }
            let ready = wake.take();
            if (ready == Some(LISTENER) || !self.listener.armed) && self.accept()? {
                self.listener.schedule(&self.port)?;
            }
            let mut deadline = native_abi::DEADLINE_INFINITE;
            for peer in &mut self.peers {
                if peer.parameters.shutdown || peer.scope.unavailable(&peer.parameters) {
                    continue;
                }
                let mut io = Io {
                    network: &self.network,
                    port: &self.port,
                    key: &mut self.key,
                    routes: &mut self.routes,
                    parameters: &peer.parameters,
                    scope: peer.scope,
                    error: &mut peer.error,
                    open_offer: &mut peer.open_offer,
                };
                if peer.outgoing.session.state() == State::Idle
                    && peer.outgoing.session.next_deadline().is_none()
                    && peer.incoming.is_none()
                    && io.open_offer.is_some()
                {
                    peer.outgoing.event(Input::Start, &mut io)?;
                }
                let mut outgoing_ready = peer
                    .outgoing
                    .transport
                    .as_ref()
                    .is_some_and(|transport| Some(transport.key) == ready);
                let mut incoming_ready = peer
                    .incoming
                    .as_ref()
                    .and_then(|connection| connection.transport.as_ref())
                    .is_some_and(|transport| Some(transport.key) == ready);
                for round in 0..WORK_BUDGET {
                    let outgoing = peer.outgoing.pump(&mut io, outgoing_ready)?;
                    let incoming = peer
                        .incoming
                        .as_mut()
                        .map(|connection| connection.pump(&mut io, incoming_ready))
                        .transpose()?
                        .unwrap_or(false);
                    if io.open_offer.is_none() {
                        // 必需能力缺失时停掉同邻居其余半开连接，不能由启动循环或另一方向重新连接。
                        peer.outgoing.stop(&mut io)?;
                        if let Some(incoming) = &mut peer.incoming {
                            incoming.stop(&mut io)?;
                        }
                    }
                    if let Some(incoming) = &mut peer.incoming {
                        let a = peer.outgoing.session.state();
                        let b = incoming.session.state();
                        if a == State::Established && b != State::Established && b != State::Idle {
                            incoming.event(Input::CollisionLost, &mut io)?;
                        } else if b == State::Established
                            && a != State::Established
                            && a != State::Idle
                        {
                            peer.outgoing.event(Input::CollisionLost, &mut io)?;
                        } else if let (Some(_), Some(remote)) = (
                            peer.outgoing.session.negotiated(),
                            incoming.session.negotiated(),
                        ) {
                            if prefer_outgoing(peer.outgoing.session.config(), remote) {
                                incoming.event(Input::CollisionLost, &mut io)?;
                            } else {
                                peer.outgoing.event(Input::CollisionLost, &mut io)?;
                            }
                        }
                    }
                    if peer
                        .incoming
                        .as_ref()
                        .is_some_and(|connection| connection.session.state() == State::Idle)
                    {
                        peer.incoming = None;
                        if peer.outgoing.session.state() == State::Idle
                            && peer.outgoing.session.next_deadline().is_none()
                            && io.open_offer.is_some()
                        {
                            // 碰撞输家的重连被取消；赢家退出后必须再推进 Start，不能无限等待已不存在的 socket。
                            deadline = native_runtime::system::monotonic();
                        }
                    }
                    if !outgoing && !incoming {
                        break;
                    }
                    outgoing_ready = outgoing;
                    incoming_ready = incoming;
                    if round + 1 == WORK_BUDGET {
                        // 主动续调度是工作消息，不消费/重武装 socket 的一次性 observer。
                        if outgoing && let Some(transport) = &mut peer.outgoing.transport {
                            transport.schedule(&self.port)?;
                        }
                        if incoming
                            && let Some(transport) = peer
                                .incoming
                                .as_mut()
                                .and_then(|connection| connection.transport.as_mut())
                        {
                            transport.schedule(&self.port)?;
                        }
                    }
                }
                for connection in core::iter::once(&peer.outgoing).chain(peer.incoming.iter()) {
                    if let Some(next) = connection.output_deadline() {
                        deadline = deadline.min(i64::try_from(next).unwrap_or(i64::MAX));
                    }
                    if let Some(next) = connection.session.next_deadline() {
                        deadline = deadline.min(i64::try_from(next).unwrap_or(i64::MAX));
                    }
                }
            }
            if self.routes_at.is_some() || !self.routes.pending().is_empty() {
                let due = *self
                    .routes_at
                    .get_or_insert_with(|| time().saturating_add(ROUTE_BATCH_NS));
                if due <= time() {
                    self.apply_routes()?;
                }
                if let Some(next) = self.routes_at {
                    deadline = deadline.min(i64::try_from(next).unwrap_or(i64::MAX));
                }
            }
            self.listener.arm(&self.port)?;
            match self.port.wait(deadline) {
                Ok(packet) if packet.key == NETWORK || packet.key == RIB => {
                    return Err(native_abi::ERR_PEER_CLOSED);
                }
                Ok(packet) if packet.key == ROUTES => {
                    self.changes.changes().signal(signals::USER_0, 0)?;
                    self.changes
                        .changes()
                        .wait_async(&self.port, ROUTES, signals::USER_0)?;
                    self.routes.reconsider_all().map_err(route_error)?;
                    if self.routes.has_networks() || self.routes.redistribution_request().is_some()
                    {
                        self.routes_at
                            .get_or_insert_with(|| time().saturating_add(ROUTE_BATCH_NS));
                    }
                }
                Ok(packet) if packet.key == CONTROL => {
                    if packet.observed & signals::PEER_CLOSED != 0 {
                        return self.stop();
                    }
                    self.control.wait_async(
                        &self.port,
                        CONTROL,
                        signals::READABLE | signals::PEER_CLOSED,
                    )?;
                }
                Ok(packet) if packet.key == LISTENER => {
                    if packet.kind == native_abi::packet::SIGNAL_ONE {
                        self.listener.notified()?;
                    } else {
                        self.listener.continued();
                    }
                    wake = Some(LISTENER);
                }
                Ok(packet) => {
                    for peer in &mut self.peers {
                        for connection in
                            core::iter::once(&mut peer.outgoing).chain(peer.incoming.iter_mut())
                        {
                            if let Some(transport) = &mut connection.transport
                                && transport.key == packet.key
                            {
                                if packet.kind == native_abi::packet::SIGNAL_ONE {
                                    transport.notified()?;
                                } else {
                                    transport.continued();
                                }
                            }
                        }
                    }
                    wake = Some(packet.key);
                }
                Err(native_abi::ERR_TIMED_OUT) => {}
                Err(error) => return Err(error),
            }
        }
    }
}
