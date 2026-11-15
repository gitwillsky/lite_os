#![no_std]

//! 网络服务（netstack）与使用者之间的协议。
//!
//! 1. 控制 channel：使用者经根服务的服务目录连接（`service_directory::NETWORK`），为每个网络域创建一条域
//!    channel（[`NetworkService::create_namespace`]）；关闭域 channel 即退休该域。
//! 2. 安装 channel（`service_directory::NETWORK_INSTALLER`，只路由给 netcfg）：委托设备目录能力与 ID，由 netstack 打开队列并安装默认网络接口
//!    （[`InstallerService::install_device`]）。
//! 3. 域 channel：创建 socket、查询接口与路由快照、配置接口与路由（[`NamespaceService`]）。
//! 4. socket 控制 channel 与共享 SQ/CQ：每 socket 一组（[`InetSocketService`]、[`PacketSocketService`]）；使用者的最后一次
//!    文件关闭发出 CLOSE，关闭 channel 同样关闭 socket（使用者终止时）；
//!    服务端在就绪状态可能变化时置位随 socket 交出的 Event 的 `USER_0`，使用者据此重新查询
//!    [`SocketPollState`]。协议引擎操作不等待网络进展；数据面调用等待实际完成，Linux 阻塞、超时与信号中断属于使用者。
//!
//! 控制线格式见 `service_rpc`；领域结果以一字节开始（0 成功，否则 [`SocketError::code`]），QUEUE 返回版本和句柄。
//! 数据面通过版本化固定共享 SQ/CQ 及持久载荷槽传输，控制 channel 只承载协商与低频操作。

extern crate alloc;

mod client;
mod client_queue;
mod queue;
mod server_queue;
mod socket_io;
pub use server_queue::SocketQueue;
mod types;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Event, Handle, Vmo};
use service_rpc::take_handle;
pub use service_rpc::{Reader, Writer};

pub use client::{NetworkClient, RemoteInetSocket, RemoteNamespace, RemotePacketSocket};
pub use types::*;

/// IPv4 UDP datagram 的最大负载（65535 - 20 字节 IP 头 - 8 字节 UDP 头）。
pub const MAX_IPV4_UDP_BYTES: usize = 65_507;
/// 一次 send/receive 传递的最大字节数：容纳最大 IPv4 datagram 与以太网帧；流式 socket 更大的读写
/// 由使用者按部分完成的语义分多次进行。
pub const TRANSFER_LIMIT: usize = 256 * 1024;

mod installer;
pub use installer::{InstallerService, NetworkInstaller, dispatch_installer};

mod ordinal {
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
    pub(super) const IS_LISTENING: u32 = 8;
    pub(super) const LISTEN: u32 = 9;
    pub(super) const ACCEPT: u32 = 10;
    pub(super) const CONNECTION_RESULT: u32 = 11;
    pub(super) const TAKE_ERROR: u32 = 12;
    pub(super) const SHUTDOWN: u32 = 13;
    pub(super) const SET_OPTION: u32 = 15;
    pub(super) const BIND_TO_DEVICE: u32 = 16;
    pub(super) const OPTION: u32 = 17;
    pub(super) const CLOSE: u32 = 18;
    pub(super) const QUEUE: u32 = 19;
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

/// 服务端：一个网络域。
pub trait NamespaceService: Send + Sync {
    fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError>;
    fn packet_socket(&self, kind: PacketKind, protocol: u16)
    -> Result<SocketEndpoint, SocketError>;
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
    fn configure_route(
        &self,
        request: Ipv4RouteRequest,
        change: RouteChange,
    ) -> Result<(), Ipv4RouteError>;
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
    writer.bytes(
        snapshot
            .driver
            .as_ref()
            .map_or("", DriverName::as_str)
            .as_bytes(),
    );
    writer.bool(snapshot.driver.is_some());
    writer.bool(snapshot.link.is_some());
    if let Some(link) = snapshot.link {
        writer
            .bool(link.carrier)
            .u32(link.speed_mbps.unwrap_or(u32::MAX));
    }
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
    let link = if reader.bool()? {
        let carrier = reader.bool()?;
        let speed = reader.u32()?;
        Some(device_model::network::NetworkLink {
            carrier,
            speed_mbps: (speed <= i32::MAX as u32).then_some(speed),
        })
    } else {
        None
    };
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
        link,
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

fn get_statistics(
    reader: &mut Reader<'_>,
) -> Result<device_model::network::NetworkStatistics, Status> {
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
        code => Ok(Err(
            SocketError::from_code(code).ok_or(native_abi::ERR_INVALID_ARGS)?
        )),
    }
}

fn endpoint_handles(endpoint: SocketEndpoint) -> Vec<Handle> {
    alloc::vec![
        endpoint.channel.into_handle(),
        endpoint.readiness.into_handle()
    ]
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
    _handles: Vec<Handle>,
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
        ordinal::SET_OPTION => {
            put_result(reply, service.set_option(get_option_change(request)?));
        }
        ordinal::BIND_TO_DEVICE => {
            let name = request.bytes()?;
            put_result(reply, service.bind_to_device(name, request.bool()?));
        }
        ordinal::OPTION => {
            let option =
                InternetOption::from_code(request.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?;
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
    _handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        ordinal::BIND => {
            put_result(reply, service.bind(get_packet_address(request)?));
        }
        ordinal::ADDRESS => {
            if let Some(address) = put_result(reply, service.address()) {
                put_packet_address(reply, address);
            }
        }
        ordinal::TAKE_ERROR => {
            reply.u8(service.take_error().map_or(0, SocketError::code));
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(Vec::new())
}

/// 服务端持有的 socket 对象：按 channel 分派到对应服务。
pub enum ServedSocket {
    Inet(Arc<dyn InetSocketService>),
    Packet(Arc<dyn PacketSocketService>),
}

impl ServedSocket {
    /// 当前 socket 协议在成功回复此方法后结束连接；传输层不解释或硬编码领域 ordinal。
    pub fn closes_connection(&self, ordinal: u32) -> bool {
        ordinal == ordinal::CLOSE
    }

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
