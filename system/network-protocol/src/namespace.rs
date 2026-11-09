//! 网络域控制协议：socket 创建、接口/FIB 观测与现有配置投影的 trait 和消息分派。

use super::*;

/// 服务端：一个网络域。
pub trait NamespaceService: Send + Sync {
    fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError>;
    fn packet_socket(&self, kind: PacketKind, protocol: u16)
    -> Result<SocketEndpoint, SocketError>;
    fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError>;
    fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError>;
    fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError>;
    fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError>;
    fn route_snapshot(&self) -> Result<RoutingSnapshot, SocketError>;
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
    _handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    let unit = |reply: &mut Writer, result: Result<(), SocketError>| {
        put_result(reply, result);
        Ok(Vec::new())
    };
    match ordinal {
        ordinal::SOCKET => {
            let protocol = match request.u8()? {
                0 => InetProtocol::Udp,
                1 => InetProtocol::Tcp,
                2 => InetProtocol::Icmp,
                3 => InetProtocol::Raw,
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            };
            Ok(put_result(reply, service.socket(protocol)).map_or_else(Vec::new, endpoint_handles))
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
        ordinal::ROUTE_SNAPSHOT => {
            request.finish()?;
            if !_handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            reply.try_reserve(9)?;
            let mut handles = Vec::new();
            handles
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            if let Some(snapshot) = put_result(reply, service.route_snapshot()) {
                let content = Vmo::from_bytes(snapshot.as_bytes())?;
                reply.u64(snapshot.as_bytes().len() as u64);
                handles.push(content.into_handle());
            }
            Ok(handles)
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
