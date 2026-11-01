#![no_std]

//! 网络服务（netd）与使用者之间的协议。
//!
//! 1. 控制 channel：使用者把设备目录中的网卡交给服务（[`NetworkService::attach_device`]），并为每个网络域
//!    创建一条域 channel（[`NetworkService::create_namespace`]）；关闭域 channel 即退休该域。
//! 2. 域 channel：创建 socket、查询接口与路由快照、配置接口与路由（[`NamespaceService`]）。
//! 3. socket channel：每个 socket 一条（[`InetSocketService`]、[`PacketSocketService`]）；使用者的最后一次
//!    文件关闭发出 CLOSE，关闭 channel 同样关闭 socket（使用者终止时）；
//!    服务端在就绪状态可能变化时置位随 socket 交出的 Event 的 `USER_0`，使用者据此重新查询
//!    [`SocketPollState`]。全部 socket 操作都不阻塞：等待、超时与信号中断属于使用者。
//!
//! 线格式与请求/回复传输见 `service_rpc`；每个回复以一个字节的结果开始（0 成功，否则 [`SocketError::code`]）。
//! 超过 [`INLINE_PAYLOAD`] 的数据经 VMO 传递，使每条消息装进一条 channel 消息。

extern crate alloc;

mod types;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Event, Handle, Vmo};
pub use service_rpc::{Reader, Writer};
use service_rpc::{call, take_handle};

pub use types::*;

/// 内联在消息中的最大数据量；更大的数据经 VMO 传递。
pub const INLINE_PAYLOAD: usize = 48 * 1024;
/// IPv4 UDP datagram 的最大负载（65535 - 20 字节 IP 头 - 8 字节 UDP 头）。
pub const MAX_IPV4_UDP_BYTES: usize = 65_507;
/// 一次 send/receive 传递的最大字节数：容纳最大 IPv4 datagram 与以太网帧；流式 socket 更大的读写
/// 由使用者按部分完成的语义分多次进行。
pub const TRANSFER_LIMIT: usize = 256 * 1024;

mod ordinal {
    pub(super) const ATTACH_DEVICE: u32 = 1;
    pub(super) const CREATE_NAMESPACE: u32 = 2;

    pub(super) const SOCKET: u32 = 1;
    pub(super) const PACKET_SOCKET: u32 = 2;
    pub(super) const INTERFACE_INDEX: u32 = 3;
    pub(super) const INTERFACE_SNAPSHOT: u32 = 4;
    pub(super) const INTERFACE_SNAPSHOTS: u32 = 5;
    pub(super) const NETWORK_SNAPSHOT: u32 = 6;
    pub(super) const ROUTE_SNAPSHOT: u32 = 7;
    pub(super) const CONFIGURE_MTU: u32 = 8;
    pub(super) const CONFIGURE_ADDRESS: u32 = 9;
    pub(super) const CONFIGURE_NETMASK: u32 = 10;
    pub(super) const CONFIGURE_BROADCAST: u32 = 11;
    pub(super) const CONFIGURE_UP: u32 = 12;
    pub(super) const CONFIGURE_ROUTE: u32 = 13;

    pub(super) const BIND: u32 = 1;
    pub(super) const CONNECT: u32 = 2;
    pub(super) const DISCONNECT: u32 = 3;
    pub(super) const ADDRESS: u32 = 4;
    pub(super) const PEER_ADDRESS: u32 = 5;
    pub(super) const SEND: u32 = 6;
    pub(super) const RECEIVE: u32 = 7;
    pub(super) const IS_LISTENING: u32 = 8;
    pub(super) const LISTEN: u32 = 9;
    pub(super) const ACCEPT: u32 = 10;
    pub(super) const CONNECTION_RESULT: u32 = 11;
    pub(super) const TAKE_ERROR: u32 = 12;
    pub(super) const SHUTDOWN: u32 = 13;
    pub(super) const POLL_STATE: u32 = 14;
    pub(super) const SET_OPTION: u32 = 15;
    pub(super) const BIND_TO_DEVICE: u32 = 16;
    pub(super) const OPTION: u32 = 17;
    pub(super) const CLOSE: u32 = 18;
}

/// 新建 socket 的两端：服务端持 channel 对端与 Event 原件，使用者持这里的 channel 与 Event 副本。
pub struct SocketEndpoint {
    pub channel: Channel,
    pub readiness: Event,
}

/// INET socket 的选项写入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InetOptionChange {
    HeaderIncluded(bool),
    PacketInfo(bool),
    ReuseAddress(bool),
    Broadcast(bool),
    /// `None` 恢复默认 TTL。
    HopLimit(Option<u8>),
    NoDelay(bool),
    /// keepalive 与 TCP 定时器类选项（`KeepAlive`…`Linger2`）。
    Timer(InternetOption, i32),
    MaxSegment(i32),
}

/// 一次 INET receive 的结果。
pub struct InetReceived {
    pub data: Vec<u8>,
    /// 消息的完整长度（datagram 截断时大于 `data.len()`）。
    pub full_length: usize,
    pub source: Option<InetAddress>,
    pub packet_info: Option<InternetPacketInfo>,
}

/// 一次 packet receive 的结果。
pub struct PacketReceived {
    pub data: Vec<u8>,
    pub full_length: usize,
    pub source: PacketAddress,
}

/// 服务端：网络服务的控制面。
pub trait NetworkService: Send + Sync {
    /// 接入设备目录中的一块网卡（目录记录的描述与句柄原样转交）。
    ///
    /// # Errors
    ///
    /// 记录无法解析或连接失败。
    fn attach_device(&self, description: Vec<u8>, handles: Vec<Handle>) -> Result<(), Status>;

    /// 创建网络域，返回交给使用者的域 channel；`initial` 的域接入全部网卡并启用回环。
    ///
    /// # Errors
    ///
    /// 资源不足。
    fn create_namespace(&self, initial: bool) -> Result<Channel, Status>;
}

/// 服务端：一个网络域。
pub trait NamespaceService: Send + Sync {
    fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError>;
    fn packet_socket(&self, kind: PacketKind, protocol: u16) -> Result<SocketEndpoint, SocketError>;
    fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError>;
    fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError>;
    fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError>;
    fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError>;
    fn route_snapshot(&self) -> Result<Vec<RouteSnapshot>, SocketError>;
    fn configure_mtu(&self, index: u32, mtu: i32) -> Result<(), SocketError>;
    fn configure_address(&self, index: u32, address: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_netmask(&self, index: u32, netmask: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_broadcast(&self, index: u32, broadcast: Ipv4Addr) -> Result<(), SocketError>;
    fn configure_up(&self, index: u32, up: bool) -> Result<(), SocketError>;
    fn configure_route(&self, request: Ipv4RouteRequest, change: RouteChange) -> Result<(), Ipv4RouteError>;
}

/// 服务端：一个 INET socket；全部操作不阻塞。
pub trait InetSocketService: Send + Sync {
    fn bind(&self, address: InetAddress) -> Result<(), SocketError>;
    fn connect(&self, peer: InetAddress) -> Result<(), SocketError>;
    fn disconnect(&self) -> Result<(), SocketError>;
    fn address(&self) -> Result<InetAddress, SocketError>;
    fn peer_address(&self) -> Result<InetAddress, SocketError>;
    fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: Option<InternetSendPacketInfo>,
    ) -> Result<usize, SocketError>;
    fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError>;
    fn is_listening(&self) -> Result<bool, SocketError>;
    fn listen(&self, backlog: usize) -> Result<(), SocketError>;
    fn accept(&self) -> Result<SocketEndpoint, SocketError>;
    fn connection_result(&self) -> Result<(), SocketError>;
    fn take_error(&self) -> Option<SocketError>;
    fn shutdown(&self, how: usize) -> Result<(), SocketError>;
    fn poll_state(&self) -> SocketPollState;
    fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError>;
    /// `privileged`：调用者在域的 owning user namespace 中具有 `CAP_NET_RAW`（已绑定后改绑需要它）。
    fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError>;
    fn option(&self, option: InternetOption) -> Result<i32, SocketError>;
    /// 使用者关闭了 socket（CLOSE 或 channel 对端关闭）；重复调用无副作用。
    fn close(&self);
}

/// 服务端：一个 packet socket；全部操作不阻塞。
pub trait PacketSocketService: Send + Sync {
    fn bind(&self, address: PacketAddress) -> Result<(), SocketError>;
    fn address(&self) -> Result<PacketAddress, SocketError>;
    fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError>;
    fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError>;
    fn take_error(&self) -> Option<SocketError>;
    fn poll_state(&self) -> SocketPollState;
    /// 使用者关闭了 socket（CLOSE 或 channel 对端关闭）；重复调用无副作用。
    fn close(&self);
}

// ---- 线编码 ----

fn put_ipv4(writer: &mut Writer, address: Ipv4Addr) {
    writer.u32(u32::from_be_bytes(address.octets()));
}

fn get_ipv4(reader: &mut Reader<'_>) -> Result<Ipv4Addr, Status> {
    Ok(Ipv4Addr::from(reader.u32()?.to_be_bytes()))
}

fn put_option_ipv4(writer: &mut Writer, address: Option<Ipv4Addr>) {
    writer.bool(address.is_some());
    put_ipv4(writer, address.unwrap_or(Ipv4Addr::UNSPECIFIED));
}

fn get_option_ipv4(reader: &mut Reader<'_>) -> Result<Option<Ipv4Addr>, Status> {
    let present = reader.bool()?;
    let address = get_ipv4(reader)?;
    Ok(present.then_some(address))
}

fn put_option_u32(writer: &mut Writer, value: Option<u32>) {
    writer.bool(value.is_some()).u32(value.unwrap_or(0));
}

fn get_option_u32(reader: &mut Reader<'_>) -> Result<Option<u32>, Status> {
    let present = reader.bool()?;
    let value = reader.u32()?;
    Ok(present.then_some(value))
}

fn put_inet(writer: &mut Writer, address: InetAddress) {
    put_ipv4(writer, address.address);
    writer.u16(address.port);
}

fn get_inet(reader: &mut Reader<'_>) -> Result<InetAddress, Status> {
    Ok(InetAddress {
        address: get_ipv4(reader)?,
        port: reader.u16()?,
    })
}

fn put_option_inet(writer: &mut Writer, address: Option<InetAddress>) {
    writer.bool(address.is_some());
    put_inet(
        writer,
        address.unwrap_or(InetAddress {
            address: Ipv4Addr::UNSPECIFIED,
            port: 0,
        }),
    );
}

fn get_option_inet(reader: &mut Reader<'_>) -> Result<Option<InetAddress>, Status> {
    let present = reader.bool()?;
    let address = get_inet(reader)?;
    Ok(present.then_some(address))
}

fn put_packet_address(writer: &mut Writer, address: PacketAddress) {
    writer
        .u16(address.protocol)
        .i32(address.interface_index)
        .u16(address.hardware_type)
        .u8(address.packet_type)
        .u8(address.address_length)
        .bytes(&address.address);
}

fn get_packet_address(reader: &mut Reader<'_>) -> Result<PacketAddress, Status> {
    let protocol = reader.u16()?;
    let interface_index = reader.i32()?;
    let hardware_type = reader.u16()?;
    let packet_type = reader.u8()?;
    let address_length = reader.u8()?;
    let address = reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    Ok(PacketAddress {
        protocol,
        interface_index,
        hardware_type,
        packet_type,
        address_length,
        address,
    })
}

fn put_send_info(writer: &mut Writer, info: Option<InternetSendPacketInfo>) {
    writer.bool(info.is_some());
    let info = info.unwrap_or_default();
    put_option_u32(writer, info.interface_index);
    put_option_ipv4(writer, info.source_address);
}

fn get_send_info(reader: &mut Reader<'_>) -> Result<Option<InternetSendPacketInfo>, Status> {
    let present = reader.bool()?;
    let info = InternetSendPacketInfo {
        interface_index: get_option_u32(reader)?,
        source_address: get_option_ipv4(reader)?,
    };
    Ok(present.then_some(info))
}

fn put_packet_info(writer: &mut Writer, info: Option<InternetPacketInfo>) {
    writer.bool(info.is_some());
    let info = info.unwrap_or(InternetPacketInfo {
        interface_index: 0,
        specific_address: Ipv4Addr::UNSPECIFIED,
        destination: Ipv4Addr::UNSPECIFIED,
    });
    writer.u32(info.interface_index);
    put_ipv4(writer, info.specific_address);
    put_ipv4(writer, info.destination);
}

fn get_packet_info(reader: &mut Reader<'_>) -> Result<Option<InternetPacketInfo>, Status> {
    let present = reader.bool()?;
    let info = InternetPacketInfo {
        interface_index: reader.u32()?,
        specific_address: get_ipv4(reader)?,
        destination: get_ipv4(reader)?,
    };
    Ok(present.then_some(info))
}

fn put_poll_state(writer: &mut Writer, state: SocketPollState) {
    writer
        .bool(state.readable)
        .bool(state.writable)
        .bool(state.hangup)
        .bool(state.read_closed)
        .bool(state.error);
}

fn get_poll_state(reader: &mut Reader<'_>) -> Result<SocketPollState, Status> {
    Ok(SocketPollState {
        readable: reader.bool()?,
        writable: reader.bool()?,
        hangup: reader.bool()?,
        read_closed: reader.bool()?,
        error: reader.bool()?,
    })
}

fn put_interface(writer: &mut Writer, snapshot: &InterfaceSnapshot) {
    writer
        .u32(snapshot.index)
        .bytes(&snapshot.name)
        .bool(snapshot.loopback)
        .i32(snapshot.mtu)
        .bytes(&snapshot.mac);
    put_option_ipv4(writer, snapshot.address);
    writer.u8(snapshot.prefix_length);
    put_ipv4(writer, snapshot.broadcast);
    writer.bool(snapshot.up);
    writer.bytes(snapshot.driver.as_ref().map_or("", DriverName::as_str).as_bytes());
    writer.bool(snapshot.driver.is_some());
}

fn fixed<const N: usize>(reader: &mut Reader<'_>) -> Result<[u8; N], Status> {
    reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)
}

fn get_interface(reader: &mut Reader<'_>) -> Result<InterfaceSnapshot, Status> {
    let index = reader.u32()?;
    let name = fixed::<16>(reader)?;
    let loopback = reader.bool()?;
    let mtu = reader.i32()?;
    let mac = fixed::<6>(reader)?;
    let address = get_option_ipv4(reader)?;
    let prefix_length = reader.u8()?;
    let broadcast = get_ipv4(reader)?;
    let up = reader.bool()?;
    let driver = core::str::from_utf8(reader.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let driver = reader.bool()?.then(|| DriverName::new(driver));
    Ok(InterfaceSnapshot {
        index,
        name,
        loopback,
        mtu,
        mac,
        address,
        prefix_length,
        broadcast,
        up,
        driver,
    })
}

fn put_statistics(writer: &mut Writer, statistics: &device_model::network::NetworkStatistics) {
    writer
        .u64(statistics.received_bytes)
        .u64(statistics.received_packets)
        .u64(statistics.received_errors)
        .u64(statistics.received_dropped)
        .u64(statistics.transmitted_bytes)
        .u64(statistics.transmitted_packets)
        .u64(statistics.transmitted_errors)
        .u64(statistics.transmitted_dropped);
}

fn get_statistics(reader: &mut Reader<'_>) -> Result<device_model::network::NetworkStatistics, Status> {
    Ok(device_model::network::NetworkStatistics {
        received_bytes: reader.u64()?,
        received_packets: reader.u64()?,
        received_errors: reader.u64()?,
        received_dropped: reader.u64()?,
        transmitted_bytes: reader.u64()?,
        transmitted_packets: reader.u64()?,
        transmitted_errors: reader.u64()?,
        transmitted_dropped: reader.u64()?,
    })
}

fn put_route(writer: &mut Writer, route: &Ipv4RouteRequest) {
    put_ipv4(writer, route.destination);
    writer
        .u8(route.prefix_length)
        .u32(route.table)
        .u32(route.metric)
        .u8(route.scope)
        .u8(route.protocol)
        .u8(route.kind.code());
    put_option_u32(writer, route.interface_index);
    put_option_ipv4(writer, route.gateway);
    put_option_ipv4(writer, route.preferred_source);
    writer
        .bool(route.on_link)
        .u32(route.metrics.advertised_mss)
        .u32(route.metrics.receive_window)
        .u32(route.metrics.round_trip_time);
}

fn get_route(reader: &mut Reader<'_>) -> Result<Ipv4RouteRequest, Status> {
    Ok(Ipv4RouteRequest {
        destination: get_ipv4(reader)?,
        prefix_length: reader.u8()?,
        table: reader.u32()?,
        metric: reader.u32()?,
        scope: reader.u8()?,
        protocol: reader.u8()?,
        kind: Ipv4RouteKind::from_code(reader.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?,
        interface_index: get_option_u32(reader)?,
        gateway: get_option_ipv4(reader)?,
        preferred_source: get_option_ipv4(reader)?,
        on_link: reader.bool()?,
        metrics: Ipv4RouteMetrics {
            advertised_mss: reader.u32()?,
            receive_window: reader.u32()?,
            round_trip_time: reader.u32()?,
        },
    })
}

/// 写入数据：不超过 [`INLINE_PAYLOAD`] 时内联，否则写入新 VMO 并把句柄追加到 `handles`。
fn put_payload(writer: &mut Writer, handles: &mut Vec<Handle>, data: &[u8]) -> Result<(), Status> {
    if data.len() <= INLINE_PAYLOAD {
        writer.bool(false).bytes(data);
        return Ok(());
    }
    let vmo = Vmo::create(data.len() as u64, 0)?;
    vmo.write(data, 0)?;
    handles.try_reserve(1).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    writer.bool(true).u64(data.len() as u64).u32(handles.len() as u32);
    handles.push(vmo.into_handle());
    Ok(())
}

/// 读出 [`put_payload`] 写入的数据。
fn get_payload(reader: &mut Reader<'_>, handles: &mut [Handle]) -> Result<Vec<u8>, Status> {
    let mut data = Vec::new();
    if !reader.bool()? {
        let bytes = reader.bytes()?;
        data.try_reserve_exact(bytes.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        data.extend_from_slice(bytes);
        return Ok(data);
    }
    let length = usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
    let index = reader.u32()? as usize;
    let vmo = Vmo::from_handle(take_handle(handles, index)?);
    data.try_reserve_exact(length)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    data.resize(length, 0);
    vmo.read(&mut data, 0)?;
    Ok(data)
}

fn put_result<T>(writer: &mut Writer, result: Result<T, SocketError>) -> Option<T> {
    match result {
        Ok(value) => {
            writer.u8(0);
            Some(value)
        }
        Err(error) => {
            writer.u8(error.code());
            None
        }
    }
}

fn get_result(reader: &mut Reader<'_>) -> Result<Result<(), SocketError>, Status> {
    match reader.u8()? {
        0 => Ok(Ok(())),
        code => Ok(Err(SocketError::from_code(code).ok_or(native_abi::ERR_INVALID_ARGS)?)),
    }
}

fn endpoint_handles(endpoint: SocketEndpoint) -> Vec<Handle> {
    alloc::vec![endpoint.channel.into_handle(), endpoint.readiness.into_handle()]
}

// ---- 服务端分派 ----

/// 分派控制 channel 的一条请求；签名与 `service_rpc::Handler` 一致。
///
/// # Errors
///
/// 未知请求、格式错误或服务的失败。
pub fn dispatch_control(
    service: &dyn NetworkService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    _reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        ordinal::ATTACH_DEVICE => {
            let description = request.bytes()?;
            let mut copy = Vec::new();
            copy.try_reserve_exact(description.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            copy.extend_from_slice(description);
            service.attach_device(copy, handles)?;
            Ok(Vec::new())
        }
        ordinal::CREATE_NAMESPACE => {
            let channel = service.create_namespace(request.bool()?)?;
            Ok(alloc::vec![channel.into_handle()])
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
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
            let kind = if request.bool()? { PacketKind::Raw } else { PacketKind::Datagram };
            let protocol = request.u16()?;
            Ok(put_result(reply, service.packet_socket(kind, protocol)).map_or_else(Vec::new, endpoint_handles))
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
            if let Some(rows) = put_result(reply, service.route_snapshot()) {
                reply.u32(rows.len() as u32);
                for row in &rows {
                    reply.bytes(&row.name);
                    put_route(reply, &row.route);
                }
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
            unit(reply, service.configure_broadcast(index, get_ipv4(request)?))
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
            reply.u8(service.configure_route(route, change).err().map_or(0, Ipv4RouteError::code));
            Ok(Vec::new())
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

fn get_option_change(request: &mut Reader<'_>) -> Result<InetOptionChange, Status> {
    let kind = request.u8()?;
    let value = request.i32()?;
    let option = request.u8()?;
    Ok(match kind {
        0 => InetOptionChange::HeaderIncluded(value != 0),
        1 => InetOptionChange::PacketInfo(value != 0),
        2 => InetOptionChange::ReuseAddress(value != 0),
        3 => InetOptionChange::Broadcast(value != 0),
        4 => InetOptionChange::HopLimit(u8::try_from(value).ok()),
        5 => InetOptionChange::NoDelay(value != 0),
        6 => InetOptionChange::Timer(
            InternetOption::from_code(option).ok_or(native_abi::ERR_INVALID_ARGS)?,
            value,
        ),
        7 => InetOptionChange::MaxSegment(value),
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    })
}

fn put_option_change(writer: &mut Writer, change: InetOptionChange) {
    let (kind, value, option) = match change {
        InetOptionChange::HeaderIncluded(enabled) => (0, i32::from(enabled), 0),
        InetOptionChange::PacketInfo(enabled) => (1, i32::from(enabled), 0),
        InetOptionChange::ReuseAddress(enabled) => (2, i32::from(enabled), 0),
        InetOptionChange::Broadcast(enabled) => (3, i32::from(enabled), 0),
        InetOptionChange::HopLimit(value) => (4, value.map_or(-1, i32::from), 0),
        InetOptionChange::NoDelay(enabled) => (5, i32::from(enabled), 0),
        InetOptionChange::Timer(option, value) => (6, value, option.code()),
        InetOptionChange::MaxSegment(value) => (7, value, 0),
    };
    writer.u8(kind).i32(value).u8(option);
}

/// 分派 INET socket channel 的一条请求。
///
/// # Errors
///
/// 未知请求或格式错误；服务的领域失败编码在回复中。
pub fn dispatch_inet(
    service: &dyn InetSocketService,
    ordinal: u32,
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    let mut reply_handles = Vec::new();
    match ordinal {
        ordinal::BIND => {
            put_result(reply, service.bind(get_inet(request)?));
        }
        ordinal::CONNECT => {
            put_result(reply, service.connect(get_inet(request)?));
        }
        ordinal::DISCONNECT => {
            put_result(reply, service.disconnect());
        }
        ordinal::ADDRESS | ordinal::PEER_ADDRESS => {
            let result = if ordinal == ordinal::ADDRESS {
                service.address()
            } else {
                service.peer_address()
            };
            if let Some(address) = put_result(reply, result) {
                put_inet(reply, address);
            }
        }
        ordinal::SEND => {
            let target = get_option_inet(request)?;
            let info = get_send_info(request)?;
            let data = get_payload(request, &mut handles)?;
            if let Some(count) = put_result(reply, service.send_to(&data, target, info)) {
                reply.u64(count as u64);
            }
        }
        ordinal::RECEIVE => {
            let limit = (request.u64()? as usize).min(TRANSFER_LIMIT);
            let peek = request.bool()?;
            if let Some(received) = put_result(reply, service.receive(limit, peek)) {
                reply.u64(received.full_length as u64);
                put_option_inet(reply, received.source);
                put_packet_info(reply, received.packet_info);
                put_payload(reply, &mut reply_handles, &received.data)?;
            }
        }
        ordinal::IS_LISTENING => {
            if let Some(listening) = put_result(reply, service.is_listening()) {
                reply.bool(listening);
            }
        }
        ordinal::LISTEN => {
            put_result(reply, service.listen(request.u64()? as usize));
        }
        ordinal::ACCEPT => {
            if let Some(endpoint) = put_result(reply, service.accept()) {
                reply_handles = endpoint_handles(endpoint);
            }
        }
        ordinal::CONNECTION_RESULT => {
            put_result(reply, service.connection_result());
        }
        ordinal::TAKE_ERROR => {
            reply.u8(service.take_error().map_or(0, SocketError::code));
        }
        ordinal::SHUTDOWN => {
            put_result(reply, service.shutdown(request.u64()? as usize));
        }
        ordinal::POLL_STATE => {
            reply.u8(0);
            put_poll_state(reply, service.poll_state());
        }
        ordinal::SET_OPTION => {
            put_result(reply, service.set_option(get_option_change(request)?));
        }
        ordinal::BIND_TO_DEVICE => {
            let name = request.bytes()?;
            put_result(reply, service.bind_to_device(name, request.bool()?));
        }
        ordinal::OPTION => {
            let option = InternetOption::from_code(request.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?;
            if let Some(value) = put_result(reply, service.option(option)) {
                reply.i32(value);
            }
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(reply_handles)
}

/// 分派 packet socket channel 的一条请求。
///
/// # Errors
///
/// 未知请求或格式错误；服务的领域失败编码在回复中。
pub fn dispatch_packet(
    service: &dyn PacketSocketService,
    ordinal: u32,
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    let mut reply_handles = Vec::new();
    match ordinal {
        ordinal::BIND => {
            put_result(reply, service.bind(get_packet_address(request)?));
        }
        ordinal::ADDRESS => {
            if let Some(address) = put_result(reply, service.address()) {
                put_packet_address(reply, address);
            }
        }
        ordinal::SEND => {
            let target = if request.bool()? {
                Some(get_packet_address(request)?)
            } else {
                None
            };
            let data = get_payload(request, &mut handles)?;
            if let Some(count) = put_result(reply, service.send_to(&data, target)) {
                reply.u64(count as u64);
            }
        }
        ordinal::RECEIVE => {
            let limit = (request.u64()? as usize).min(TRANSFER_LIMIT);
            let peek = request.bool()?;
            if let Some(received) = put_result(reply, service.receive(limit, peek)) {
                reply.u64(received.full_length as u64);
                put_packet_address(reply, received.source);
                put_payload(reply, &mut reply_handles, &received.data)?;
            }
        }
        ordinal::TAKE_ERROR => {
            reply.u8(service.take_error().map_or(0, SocketError::code));
        }
        ordinal::POLL_STATE => {
            reply.u8(0);
            put_poll_state(reply, service.poll_state());
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(reply_handles)
}

// ---- 使用者端 ----

/// 传输失败的领域投影：服务内存不足为 `NoMemory`，服务不可用（已终止）为 `NetworkDown`。
fn transport(status: Status) -> SocketError {
    if status == native_abi::ERR_NO_MEMORY {
        SocketError::NoMemory
    } else {
        SocketError::NetworkDown
    }
}

/// 发出请求并解出结果字节；领域失败与传输失败合并为 [`SocketError`]。
fn request(
    channel: &Channel,
    writer: &Writer,
    handles: Vec<Handle>,
) -> Result<service_rpc::Reply, SocketError> {
    let reply = call(channel, writer, handles).map_err(transport)?;
    get_result(&mut reply.reader())
        .map_err(transport)?
        .map(|()| reply)
}

/// 跳过结果字节后的回复读取器。
fn body(reply: &service_rpc::Reply) -> Reader<'_> {
    let mut reader = reply.reader();
    let _ = reader.u8();
    reader
}

fn take_endpoint(mut reply: service_rpc::Reply) -> Result<SocketEndpoint, SocketError> {
    let channel = Channel::from_handle(take_handle(&mut reply.handles, 0).map_err(transport)?);
    let readiness = Event::from_handle(take_handle(&mut reply.handles, 1).map_err(transport)?);
    Ok(SocketEndpoint { channel, readiness })
}

/// 使用者端：网络服务的控制 channel。
pub struct NetworkClient {
    channel: Channel,
}

impl NetworkClient {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 把一条网卡目录记录转交给服务。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn attach_device(&self, description: &[u8], handles: Vec<Handle>) -> Result<(), Status> {
        let mut writer = Writer::request(ordinal::ATTACH_DEVICE);
        writer.bytes(description);
        call(&self.channel, &writer, handles).map(drop)
    }

    /// 创建网络域。
    ///
    /// # Errors
    ///
    /// 服务资源不足或已终止。
    pub fn create_namespace(&self, initial: bool) -> Result<RemoteNamespace, Status> {
        let mut writer = Writer::request(ordinal::CREATE_NAMESPACE);
        writer.bool(initial);
        let mut reply = call(&self.channel, &writer, Vec::new())?;
        Ok(RemoteNamespace {
            channel: Channel::from_handle(take_handle(&mut reply.handles, 0)?),
        })
    }
}

/// 使用者端：一个网络域；drop 即退休该域。
pub struct RemoteNamespace {
    channel: Channel,
}

impl RemoteNamespace {
    pub fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError> {
        let mut writer = Writer::request(ordinal::SOCKET);
        writer.u8(match protocol {
            InetProtocol::Udp => 0,
            InetProtocol::Tcp => 1,
            InetProtocol::Icmp => 2,
            InetProtocol::Raw => 3,
        });
        take_endpoint(request(&self.channel, &writer, Vec::new())?)
    }

    pub fn packet_socket(&self, kind: PacketKind, protocol: u16) -> Result<SocketEndpoint, SocketError> {
        let mut writer = Writer::request(ordinal::PACKET_SOCKET);
        writer.bool(kind == PacketKind::Raw).u16(protocol);
        take_endpoint(request(&self.channel, &writer, Vec::new())?)
    }

    pub fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError> {
        let mut writer = Writer::request(ordinal::INTERFACE_INDEX);
        writer.bytes(name);
        let reply = request(&self.channel, &writer, Vec::new())?;
        body(&reply).u32().map_err(transport)
    }

    pub fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError> {
        let mut writer = Writer::request(ordinal::INTERFACE_SNAPSHOT);
        writer.u32(index);
        let reply = request(&self.channel, &writer, Vec::new())?;
        get_interface(&mut body(&reply)).map_err(transport)
    }

    fn rows<T>(
        &self,
        ordinal: u32,
        decode: impl Fn(&mut Reader<'_>) -> Result<T, Status>,
    ) -> Result<Vec<T>, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal), Vec::new())?;
        let mut reader = body(&reply);
        let count = reader.u32().map_err(transport)? as usize;
        let mut rows = Vec::new();
        rows.try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        for _ in 0..count {
            rows.push(decode(&mut reader).map_err(transport)?);
        }
        Ok(rows)
    }

    pub fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError> {
        self.rows(ordinal::INTERFACE_SNAPSHOTS, get_interface)
    }

    pub fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError> {
        self.rows(ordinal::NETWORK_SNAPSHOT, |reader| {
            Ok(NetworkSnapshot {
                name: fixed::<16>(reader)?,
                statistics: get_statistics(reader)?,
            })
        })
    }

    pub fn route_snapshot(&self) -> Result<Vec<RouteSnapshot>, SocketError> {
        self.rows(ordinal::ROUTE_SNAPSHOT, |reader| {
            Ok(RouteSnapshot {
                name: fixed::<16>(reader)?,
                route: get_route(reader)?,
            })
        })
    }

    fn configure(&self, ordinal: u32, index: u32, encode: impl FnOnce(&mut Writer)) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal);
        writer.u32(index);
        encode(&mut writer);
        request(&self.channel, &writer, Vec::new()).map(drop)
    }

    pub fn configure_mtu(&self, index: u32, mtu: i32) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_MTU, index, |writer| {
            writer.i32(mtu);
        })
    }

    pub fn configure_address(&self, index: u32, address: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_ADDRESS, index, |writer| put_ipv4(writer, address))
    }

    pub fn configure_netmask(&self, index: u32, netmask: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_NETMASK, index, |writer| put_ipv4(writer, netmask))
    }

    pub fn configure_broadcast(&self, index: u32, broadcast: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_BROADCAST, index, |writer| put_ipv4(writer, broadcast))
    }

    pub fn configure_up(&self, index: u32, up: bool) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_UP, index, |writer| {
            writer.bool(up);
        })
    }

    /// # Errors
    ///
    /// FIB 准入失败；服务不可用时为 `NetworkUnreachable`，内存不足为 `NoMemory`。
    pub fn configure_route(&self, route: Ipv4RouteRequest, change: RouteChange) -> Result<(), Ipv4RouteError> {
        let mut writer = Writer::request(ordinal::CONFIGURE_ROUTE);
        put_route(&mut writer, &route);
        writer
            .bool(change.delete)
            .bool(change.replace)
            .bool(change.create)
            .bool(change.exclusive)
            .bool(change.append);
        let unavailable = |status| {
            if status == native_abi::ERR_NO_MEMORY {
                Ipv4RouteError::NoMemory
            } else {
                Ipv4RouteError::NetworkUnreachable
            }
        };
        let reply = call(&self.channel, &writer, Vec::new()).map_err(unavailable)?;
        match reply.reader().u8().map_err(unavailable)? {
            0 => Ok(()),
            code => Err(Ipv4RouteError::from_code(code).ok_or(Ipv4RouteError::Invalid)?),
        }
    }
}

fn get_error(reply: &service_rpc::Reply) -> Option<SocketError> {
    reply.reader().u8().ok().and_then(SocketError::from_code)
}

/// 使用者端：一个 INET socket；drop 即关闭。
pub struct RemoteInetSocket {
    channel: Channel,
}

impl RemoteInetSocket {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    fn unit(&self, writer: &Writer) -> Result<(), SocketError> {
        request(&self.channel, writer, Vec::new()).map(drop)
    }

    pub fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND);
        put_inet(&mut writer, address);
        self.unit(&writer)
    }

    pub fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::CONNECT);
        put_inet(&mut writer, peer);
        self.unit(&writer)
    }

    pub fn disconnect(&self) -> Result<(), SocketError> {
        self.unit(&Writer::request(ordinal::DISCONNECT))
    }

    pub fn address(&self) -> Result<InetAddress, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal::ADDRESS), Vec::new())?;
        get_inet(&mut body(&reply)).map_err(transport)
    }

    pub fn peer_address(&self) -> Result<InetAddress, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal::PEER_ADDRESS), Vec::new())?;
        get_inet(&mut body(&reply)).map_err(transport)
    }

    pub fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: Option<InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        let mut writer = Writer::request(ordinal::SEND);
        put_option_inet(&mut writer, target);
        put_send_info(&mut writer, packet_info);
        let mut handles = Vec::new();
        put_payload(&mut writer, &mut handles, input).map_err(transport)?;
        let reply = request(&self.channel, &writer, handles)?;
        body(&reply).u64().map(|count| count as usize).map_err(transport)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError> {
        let mut writer = Writer::request(ordinal::RECEIVE);
        writer.u64(limit as u64).bool(peek);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let (full_length, source, packet_info, data) = {
            let (mut reader, handles) = reply.split();
            let _ = reader.u8();
            let full_length = reader.u64().map_err(transport)? as usize;
            let source = get_option_inet(&mut reader).map_err(transport)?;
            let packet_info = get_packet_info(&mut reader).map_err(transport)?;
            let data = get_payload(&mut reader, handles).map_err(transport)?;
            (full_length, source, packet_info, data)
        };
        Ok(InetReceived {
            data,
            full_length,
            source,
            packet_info,
        })
    }

    pub fn is_listening(&self) -> Result<bool, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal::IS_LISTENING), Vec::new())?;
        body(&reply).bool().map_err(transport)
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::LISTEN);
        writer.u64(backlog as u64);
        self.unit(&writer)
    }

    pub fn accept(&self) -> Result<SocketEndpoint, SocketError> {
        take_endpoint(request(&self.channel, &Writer::request(ordinal::ACCEPT), Vec::new())?)
    }

    pub fn connection_result(&self) -> Result<(), SocketError> {
        self.unit(&Writer::request(ordinal::CONNECTION_RESULT))
    }

    pub fn take_error(&self) -> Option<SocketError> {
        match call(&self.channel, &Writer::request(ordinal::TAKE_ERROR), Vec::new()) {
            Ok(reply) => get_error(&reply),
            Err(status) => Some(transport(status)),
        }
    }

    pub fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::SHUTDOWN);
        writer.u64(how as u64);
        self.unit(&writer)
    }

    /// 服务不可用时报告错误状态。
    pub fn poll_state(&self) -> SocketPollState {
        request(&self.channel, &Writer::request(ordinal::POLL_STATE), Vec::new())
            .and_then(|reply| get_poll_state(&mut body(&reply)).map_err(transport))
            .unwrap_or_else(|_| SocketPollState::error())
    }

    pub fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::SET_OPTION);
        put_option_change(&mut writer, change);
        self.unit(&writer)
    }

    pub fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND_TO_DEVICE);
        writer.bytes(name).bool(privileged);
        self.unit(&writer)
    }

    pub fn option(&self, option: InternetOption) -> Result<i32, SocketError> {
        let mut writer = Writer::request(ordinal::OPTION);
        writer.u8(option.code());
        let reply = request(&self.channel, &writer, Vec::new())?;
        body(&reply).i32().map_err(transport)
    }

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}

/// 使用者端：一个 packet socket；drop 即关闭。
pub struct RemotePacketSocket {
    channel: Channel,
}

impl RemotePacketSocket {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    pub fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND);
        put_packet_address(&mut writer, address);
        request(&self.channel, &writer, Vec::new()).map(drop)
    }

    pub fn address(&self) -> Result<PacketAddress, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal::ADDRESS), Vec::new())?;
        get_packet_address(&mut body(&reply)).map_err(transport)
    }

    pub fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError> {
        let mut writer = Writer::request(ordinal::SEND);
        writer.bool(target.is_some());
        if let Some(target) = target {
            put_packet_address(&mut writer, target);
        }
        let mut handles = Vec::new();
        put_payload(&mut writer, &mut handles, input).map_err(transport)?;
        let reply = request(&self.channel, &writer, handles)?;
        body(&reply).u64().map(|count| count as usize).map_err(transport)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError> {
        let mut writer = Writer::request(ordinal::RECEIVE);
        writer.u64(limit as u64).bool(peek);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let (mut reader, handles) = reply.split();
        let _ = reader.u8();
        let full_length = reader.u64().map_err(transport)? as usize;
        let source = get_packet_address(&mut reader).map_err(transport)?;
        let data = get_payload(&mut reader, handles).map_err(transport)?;
        Ok(PacketReceived {
            data,
            full_length,
            source,
        })
    }

    pub fn take_error(&self) -> Option<SocketError> {
        match call(&self.channel, &Writer::request(ordinal::TAKE_ERROR), Vec::new()) {
            Ok(reply) => get_error(&reply),
            Err(status) => Some(transport(status)),
        }
    }

    pub fn poll_state(&self) -> SocketPollState {
        request(&self.channel, &Writer::request(ordinal::POLL_STATE), Vec::new())
            .and_then(|reply| get_poll_state(&mut body(&reply)).map_err(transport))
            .unwrap_or_else(|_| SocketPollState::error())
    }

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}

/// 服务端持有的 socket 对象：按 channel 分派到对应服务。
pub enum ServedSocket {
    Inet(Arc<dyn InetSocketService>),
    Packet(Arc<dyn PacketSocketService>),
}

impl ServedSocket {
    /// 分派一条请求。
    ///
    /// # Errors
    ///
    /// 未知请求或格式错误。
    pub fn dispatch(
        &self,
        ordinal: u32,
        request: &mut Reader<'_>,
        handles: Vec<Handle>,
        reply: &mut Writer,
    ) -> Result<Vec<Handle>, Status> {
        match self {
            Self::Inet(socket) => dispatch_inet(&**socket, ordinal, request, handles, reply),
            Self::Packet(socket) => dispatch_packet(&**socket, ordinal, request, handles, reply),
        }
    }

    /// 使用者关闭了 socket。
    pub fn close(&self) {
        match self {
            Self::Inet(socket) => socket.close(),
            Self::Packet(socket) => socket.close(),
        }
    }
}
