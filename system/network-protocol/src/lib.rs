#![no_std]

//! 网络服务（netstack）与使用者之间的协议。
//!
//! 1. 控制 channel：使用者经根服务的服务目录连接（`service_directory::NETWORK`），为每个网络域创建一条域
//!    channel（[`NetworkService::create_namespace`]）；关闭域 channel 即退休该域。
//! 2. 原生 RIB 独占执行安装能力；router-config 经 [`routing`] 提交接口意图，由 RIB 选路并委托 netstack 安装设备与执行批次。
//! 3. 域 channel：创建 socket、查询接口与路由快照、配置接口与路由（[`NamespaceService`]）。
//! 4. socket 控制 channel 与共享 SQ/CQ：每 socket 一组（[`InetSocketService`]、[`PacketSocketService`]）；使用者的最后一次
//!    文件关闭发出 CLOSE，关闭 channel 同样关闭 socket（使用者终止时）；
//!    服务端在就绪状态可能变化时置位随 socket 交出的 Event 的 `USER_0`，使用者据此重新查询
//!    [`SocketPollState`]。协议引擎操作不等待网络进展；数据面调用等待实际完成，Linux 阻塞、超时与信号中断属于使用者。
//!
//! 控制线格式见 `service_rpc`；领域结果以一字节开始（0 成功，否则 [`SocketError::code`]），QUEUE 返回版本和句柄。
//! 数据面通过版本化固定共享 SQ/CQ 及持久载荷槽传输，控制 channel 只承载协商与低频操作。

extern crate alloc;
pub mod bgp;
pub mod ospf;

mod address;
use address::{
    get_inet, get_ipv4, get_option_ipv4, get_packet_address, put_inet, put_ipv4, put_option_ipv4,
    put_packet_address,
};
mod client;
mod client_queue;
mod queue;
mod server_queue;
mod socket_io;
pub use server_queue::SocketQueue;
mod options;
mod packet;
pub use packet::{CaptureRing, FrameRingLayout, PacketSocketService, dispatch_packet};
mod types;
pub use options::{
    IcmpFilterUpdate, InetOptionChange, TCP_AO_MAX_KEY_BYTES, TCP_MD5_MAX_KEY_BYTES,
    TcpAoAlgorithm, TcpAoCounters, TcpAoKey, TcpAoSelection, TcpAoStatus, TcpKeySelector,
};
use options::{get_option_change, put_option_change};

use alloc::{sync::Arc, vec::Vec};
use core::net::Ipv4Addr;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Event, Handle, Vmo};
use service_rpc::take_handle;
pub use service_rpc::{Reader, Writer};

pub use client::{NetworkClient, RemoteInetSocket, RemoteNamespace, RemotePacketSocket};
pub use types::*;

/// IPv4 UDP datagram 的最大负载（65535 - 20 字节 IP 头 - 8 字节 UDP 头）。
pub const MAX_IPV4_UDP_BYTES: usize = 65_507;
/// 非 Jumbo IPv6 UDP 最大载荷（65535 字节 IPv6 payload 减去 8 字节 UDP header）。
pub const MAX_IPV6_UDP_BYTES: usize = 65_527;
/// 一次 send/receive 传递的最大字节数：容纳最大 IPv4 datagram 与以太网帧；流式 socket 更大的读写
/// 由使用者按部分完成的语义分多次进行。
pub const TRANSFER_LIMIT: usize = 256 * 1024;

mod interface;
use interface::{get_interface, put_interface};

mod namespace;
pub use namespace::{NamespaceService, NetworkService, dispatch_control, dispatch_namespace};

mod installer;
pub use installer::{InstallerService, NetworkInstaller, dispatch_installer};
mod configuration;
pub mod routes;
pub mod routing;
pub use configuration::{
    ConfigurationService, ExecutionPolicy, Forwarding, PreparedConfiguration,
    dispatch_configuration,
};
use routes::{get_route, put_route};

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
    pub(super) const MD5_KEY: u32 = 20;
    pub(super) const ICMP_FILTER: u32 = 21;
    pub(super) const AO_KEY: u32 = 22;
    pub(super) const AO_STATUS: u32 = 23;
}

mod inet;
pub use inet::{InetSocketService, TcpInformation, TcpPhase, dispatch_inet};

// ---- 线编码 ----

fn fixed<const N: usize>(reader: &mut Reader<'_>) -> Result<[u8; N], Status> {
    reader
        .bytes()?
        .try_into()
        .map_err(|_| native_abi::ERR_INVALID_ARGS)
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
