//! 网络域控制协议：socket 创建、接口/FIB 观测与现有配置投影的 trait 和消息分派。

use super::*;
pub(super) mod addresses;
pub(super) mod neighbors;
pub(super) const ROUTE_SNAPSHOT_VERSION: u32 = 1;
pub(super) const SOCKET_REQUEST_VERSION: u32 = 1;
pub(super) const LEARNED_DNS: u32 = 16;
pub(super) const WATCH_DNS: u32 = 17;
pub(super) const ROUTE_LOOKUP: u32 = 18;
pub(super) const RULE_SNAPSHOT: u32 = 19;
pub(super) const RULE_SNAPSHOT_VERSION: u32 = 1;

/// 服务端：网络服务的控制面。
pub trait NetworkService: Send + Sync {
    /// 返回交给使用者的域 channel。`initial` 时连上 netstack 的默认网络（持有全部网卡、启用回环，
    /// 随 netstack 存续，多个连接共享同一网络）；否则创建只有 DOWN 回环的新网络域，随其 channel 退休。
    ///
    /// # Errors
    ///
    /// 资源不足。
    fn create_namespace(&self, initial: bool) -> Result<Channel, Status>;
}

/// 分派控制 channel 的一条请求；签名与 `service_rpc::Handler` 一致。
///
/// # Errors
///
/// 未知请求、格式错误或服务的失败。
pub fn dispatch_control(
    service: &dyn NetworkService,
    ordinal: u32,
    request: &mut Reader<'_>,
    _handles: Vec<Handle>,
    _reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        ordinal::CREATE_NAMESPACE => {
            let channel = service.create_namespace(request.bool()?)?;
            Ok(alloc::vec![channel.into_handle()])
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

/// 服务端：一个网络域。
pub trait NamespaceService: Send + Sync {
    /// 在本域创建指定地址族 `family`、传输协议 `protocol` 的端点，返回独占 channel 和 readiness Event。
    /// # Errors
    /// 当前未实现的族/协议返回领域错误；资源不足时不发布半个端点。
    fn socket(
        &self,
        family: InetFamily,
        protocol: InetProtocol,
    ) -> Result<SocketEndpoint, SocketError>;
    fn packet_socket(&self, kind: PacketKind, protocol: u16)
    -> Result<SocketEndpoint, SocketError>;
    fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError>;
    fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError>;
    fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError>;
    fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError>;
    /// 返回本域实际 IPv4 和 IPv6 邻居的冻结状态；不触发解析或计时推进。
    /// # Parameters
    /// `index` 为原生逻辑接口，0 表示全部接口；不携带 Linux ifindex 位宽限制。
    /// # Errors
    /// 域不可用或捕获存储不足，不返回部分集合。
    fn neighbor_snapshot(&self, index: u32) -> Result<NeighborSnapshot, SocketError>;
    /// 冻结本域全部或指定接口的实际地址；未知接口或资源失败返回领域错误。
    fn address_snapshot(&self, index: u32) -> Result<AddressSnapshot, SocketError>;
    /// 冻结当前网络域有效的通告 DNS。资源不足时返回领域错误。
    fn learned_dns(&self) -> Result<router_protocol::dns::LearnedDns, SocketError>;
    /// 为配置服务登记一个通告 DNS 观察事件。重连可替换旧观察者。
    fn watch_dns(&self) -> Result<Event, SocketError>;
    /// 捕获指定族的唯一 FIB 正文；None 同时捕获两族并按 IPv4/IPv6 返回，不能跨发布代际。
    /// 只读查询不驱动路由或邻居状态。
    /// # Errors
    /// 域不可用或捕获资源不足，不返回部分集合。
    fn route_snapshot(&self, family: Option<InetFamily>) -> Result<Vec<Vec<u8>>, SocketError>;
    /// 查询本机发送或指定入接口的实际路径，不创建邻接、报文或写租约。
    /// # Errors
    /// 源地址、接口、路由不可用或域已退出。
    fn route_lookup(&self, query: routes::RouteLookup) -> Result<routes::RoutePath, SocketError>;
    /// 冻结一个或两个地址族的生效选表规则和入接口名。
    /// # Errors
    /// 域不可用或资源不足，不交付部分规则。
    fn rule_snapshot(
        &self,
        family: Option<InetFamily>,
    ) -> Result<routes::RuleSnapshot, SocketError>;
    fn configure_mtu(&self, index: u32, mtu: i32) -> Result<(), SocketError>;
    fn configure_address(&self, index: u32, address: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_netmask(&self, index: u32, netmask: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_broadcast(&self, index: u32, broadcast: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_up(&self, index: u32, up: bool) -> Result<(), SocketError>;
    fn configure_route(
        &self,
        request: Ipv4RouteRequest,
        change: RouteChange,
    ) -> Result<(), Ipv4RouteError>;
}

/// 分派域 channel 的一条请求。
///
/// # Errors
///
/// 未知请求或格式错误；服务的领域失败编码在回复中。
pub fn dispatch_namespace(
    service: &dyn NamespaceService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    let unit = |reply: &mut Writer, result: Result<(), SocketError>| {
        put_result(reply, result);
        Ok(Vec::new())
    };
    match ordinal {
        ordinal::SOCKET => {
            if request.u32()? != SOCKET_REQUEST_VERSION || !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let family = match request.u8()? {
                4 => InetFamily::Ipv4,
                6 => InetFamily::Ipv6,
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            };
            let protocol = match request.u8()? {
                0 => InetProtocol::Udp,
                1 => InetProtocol::Tcp,
                2 => {
                    let protocol = request.u8()?;
                    if protocol == 0 {
                        return Err(native_abi::ERR_INVALID_ARGS);
                    }
                    InetProtocol::Raw(protocol)
                }
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            };
            request.finish()?;
            reply.try_reserve(1)?;
            Ok(put_result(reply, service.socket(family, protocol))
                .map_or_else(Vec::new, endpoint_handles))
        }
        ordinal::PACKET_SOCKET => {
            let kind = if request.bool()? {
                PacketKind::Raw
            } else {
                PacketKind::Datagram
            };
            let protocol = request.u16()?;
            Ok(put_result(reply, service.packet_socket(kind, protocol))
                .map_or_else(Vec::new, endpoint_handles))
        }
        ordinal::INTERFACE_INDEX => {
            if let Some(index) = put_result(reply, service.interface_index(request.bytes()?)) {
                reply.u32(index);
            }
            Ok(Vec::new())
        }
        ordinal::INTERFACE_SNAPSHOT => {
            if let Some(snapshot) = put_result(reply, service.interface_snapshot(request.u32()?)) {
                put_interface(reply, &snapshot);
            }
            Ok(Vec::new())
        }
        ordinal::INTERFACE_SNAPSHOTS => {
            if let Some(rows) = put_result(reply, service.interface_snapshots()) {
                reply.u32(rows.len() as u32);
                for row in &rows {
                    put_interface(reply, row);
                }
            }
            Ok(Vec::new())
        }
        ordinal::NETWORK_SNAPSHOT => {
            if let Some(rows) = put_result(reply, service.network_snapshot()) {
                reply.u32(rows.len() as u32);
                for row in &rows {
                    reply.bytes(&row.name);
                    put_statistics(reply, &row.statistics);
                }
            }
            Ok(Vec::new())
        }
        neighbors::ORDINAL | addresses::ORDINAL => {
            let index = request.u32()?;
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            if ordinal == neighbors::ORDINAL {
                snapshot_reply(
                    reply,
                    || service.neighbor_snapshot(index),
                    NeighborSnapshot::as_bytes,
                )
            } else {
                snapshot_reply(
                    reply,
                    || service.address_snapshot(index),
                    AddressSnapshot::as_bytes,
                )
            }
        }
        RULE_SNAPSHOT => {
            if request.u32()? != RULE_SNAPSHOT_VERSION {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let family = snapshot_family(request)?;
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            snapshot_reply(
                reply,
                || service.rule_snapshot(family),
                routes::RuleSnapshot::as_bytes,
            )
        }
        LEARNED_DNS | WATCH_DNS => {
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            reply.try_reserve(9)?;
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            if ordinal == WATCH_DNS {
                if let Some(event) = put_result(reply, service.watch_dns()) {
                    returned.push(event.into_handle());
                }
            } else if let Some(dns) = put_result(reply, service.learned_dns()) {
                let mut encoder = router_protocol::Encoder::default();
                encoder
                    .0
                    .try_reserve_exact(dns.encoded_len())
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                encoder.learned_dns(&dns);
                let content = Vmo::from_bytes(&encoder.0)?;
                reply.u64(encoder.0.len() as u64);
                returned.push(content.into_handle());
            }
            Ok(returned)
        }
        ordinal::ROUTE_SNAPSHOT => {
            if request.u32()? != ROUTE_SNAPSHOT_VERSION {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let family = snapshot_family(request)?;
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let expected = if family.is_none() { 2 } else { 1 };
            reply.try_reserve(5 + expected * 8)?;
            let mut handles = Vec::new();
            handles
                .try_reserve_exact(expected)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            if let Some(snapshots) = put_result(reply, service.route_snapshot(family)) {
                if snapshots.len() != expected {
                    return Err(native_abi::ERR_BAD_STATE);
                }
                reply.u32(expected as u32);
                for snapshot in snapshots {
                    let content = Vmo::from_bytes(&snapshot)?;
                    reply.u64(snapshot.len() as u64);
                    handles.push(content.into_handle());
                }
            }
            Ok(handles)
        }
        ROUTE_LOOKUP => {
            let query = routes::RouteLookup::read(request)?;
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            reply.try_reserve(44)?;
            if let Some(path) = put_result(reply, service.route_lookup(query)) {
                path.write(reply);
            }
            Ok(Vec::new())
        }
        ordinal::CONFIGURE_MTU => {
            let index = request.u32()?;
            unit(reply, service.configure_mtu(index, request.i32()?))
        }
        ordinal::CONFIGURE_ADDRESS => {
            let index = request.u32()?;
            unit(reply, service.configure_address(index, get_ipv4(request)?))
        }
        ordinal::CONFIGURE_NETMASK => {
            let index = request.u32()?;
            unit(reply, service.configure_netmask(index, get_ipv4(request)?))
        }
        ordinal::CONFIGURE_BROADCAST => {
            let index = request.u32()?;
            unit(
                reply,
                service.configure_broadcast(index, get_ipv4(request)?),
            )
        }
        ordinal::CONFIGURE_UP => {
            let index = request.u32()?;
            unit(reply, service.configure_up(index, request.bool()?))
        }
        ordinal::CONFIGURE_ROUTE => {
            let route = get_route(request)?;
            let change = RouteChange {
                delete: request.bool()?,
                replace: request.bool()?,
                create: request.bool()?,
                exclusive: request.bool()?,
                append: request.bool()?,
            };
            reply.u8(service
                .configure_route(route, change)
                .err()
                .map_or(0, Ipv4RouteError::code));
            Ok(Vec::new())
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

fn snapshot_family(request: &mut Reader<'_>) -> Result<Option<InetFamily>, Status> {
    match request.u8()? {
        0 => Ok(None),
        4 => Ok(Some(InetFamily::Ipv4)),
        6 => Ok(Some(InetFamily::Ipv6)),
        _ => Err(native_abi::ERR_INVALID_ARGS),
    }
}

fn snapshot_reply<S>(
    reply: &mut Writer,
    capture: impl FnOnce() -> Result<S, SocketError>,
    as_bytes: impl FnOnce(&S) -> &[u8],
) -> Result<Vec<Handle>, Status> {
    reply.try_reserve(9)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    if let Some(snapshot) = put_result(reply, capture()) {
        let bytes = as_bytes(&snapshot);
        let content = Vmo::from_bytes(bytes)?;
        reply.u64(bytes.len() as u64);
        handles.push(content.into_handle());
    }
    Ok(handles)
}
