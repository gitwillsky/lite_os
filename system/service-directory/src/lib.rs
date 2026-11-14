#![no_std]
#![feature(allocator_ext)]

//! 服务目录：按协议名建立到服务的新连接（对应 Fuchsia 的 outgoing 目录与 `/svc`）。
//!
//! 1. 每个服务启动时拿到 outgoing channel 的服务端，按协议名接受连接：请求携带新连接的服务端。
//! 2. 根服务持有各服务 outgoing 的客户端，并以同一协议为消费者服务路由目录：把连接请求转发给
//!    当前提供该协议的实例。被依赖的服务重启后，新连接自然到达新实例。
//! 3. 连接的寿命即会话的寿命：提供者终止时消费者的连接 `PEER_CLOSED`，消费者重新 `connect`。

extern crate alloc;

/// BGP 域配置工厂；只路由到原生配置 owner，不直接授予 Linux 程序。
pub const BGP_CONFIGURATOR: &[u8] = b"liteos.bgp.Configuration";
/// 仅由路由配置 owner 获得的 OSPF 实例工厂。
pub const OSPF_CONFIGURATOR: &[u8] = b"liteos.ospf.Configuration";

use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle};
use service_rpc::{Reader, Writer, call, serve, take_handle};

/// netstack 的网络控制协议（`network_protocol::NetworkClient`）。
pub const NETWORK: &[u8] = b"liteos.network.Control";
/// netstack 的网卡/FIB 执行协议（`network_protocol::NetworkInstaller`），只路由给原生 RIB。
pub const NETWORK_INSTALLER: &[u8] = b"liteos.network.Installer";
/// 原生 RIB 的接口配置来源能力，只授予 router-config，不允许声明任意 FIB 或来源身份。
pub const ROUTING_CONFIGURATOR: &[u8] = b"liteos.routing.Configuration";
/// router_config 的网络设置协议（`router_protocol`），linuxd 适配给获授权的 Linux 程序。
pub const NETWORK_SETTINGS: &[u8] = b"liteos.network.Settings";
/// 驱动更新控制，只路由给 linuxd 的管理员端点。
pub const DRIVER_UPDATE: &[u8] = b"liteos.driver.Update";
/// dhcp-client 的 DHCPv4 租约协议（`router_protocol::dhcp`），只路由给 router_config。
pub const DHCP: &[u8] = b"liteos.network.Dhcp";
/// DHCPv4 下游地址分发执行能力，只授予 router-config。
pub const DHCP_SERVER: &[u8] = b"liteos.network.DhcpServer";
/// DNS 转发执行工厂，仅授权给路由配置 owner。
pub const DNS_CACHE: &[u8] = b"liteos.network.DnsCache";
/// fshost 的存储管理协议（`filesystem_protocol::StorageAdmin`）：只供根服务为各服务派生私有数据目录。
pub const STORAGE_ADMIN: &[u8] = b"liteos.storage.Admin";
/// fshost 的块设备清单协议（`filesystem_protocol::BlocksClient`）。
pub const BLOCKS: &[u8] = b"liteos.storage.Blocks";
/// fshost 的卷挂载协议（`filesystem_protocol::VolumeMountClient`）。
pub const EXT4: &[u8] = b"liteos.filesystem.Ext4";
/// audio-core 的音频协议（`audio_protocol::AudioClient`）。
pub const AUDIO: &[u8] = b"liteos.audio.Core";

const CONNECT: u32 = 1;
/// 协议名上限；超过即为格式错误。
const NAME_MAX: usize = 64;

/// 连接处理函数：协议名与新连接的服务端；未知协议返回 `ERR_NOT_FOUND`。
pub type Connector = dyn Fn(&[u8], Channel) -> Result<(), Status> + Send + Sync;

/// 服务一条目录 channel。
///
/// # Errors
///
/// 服务线程或闭包分配失败。
pub fn serve_directory(
    channel: Channel,
    name: &[u8],
    connector: Arc<Connector>,
) -> Result<(), Status> {
    serve(
        channel,
        1,
        name,
        Arc::try_new(
            move |ordinal: u32,
                  request: &mut Reader<'_>,
                  mut handles: Vec<Handle>,
                  _: &mut Writer| {
                if ordinal != CONNECT {
                    return Err(native_abi::ERR_NOT_SUPPORTED);
                }
                let protocol = request.bytes()?;
                if protocol.len() > NAME_MAX {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                connector(
                    protocol,
                    Channel::from_handle(take_handle(&mut handles, 0)?),
                )?;
                Ok(Vec::new())
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )
}

/// 使用者端的服务目录。
pub struct ServiceDirectory(Channel);

impl ServiceDirectory {
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }

    /// 建立到 `protocol` 的新连接，返回客户端一端。
    ///
    /// # Errors
    ///
    /// 目录已关闭、协议不存在（`ERR_NOT_FOUND`）或提供者拒绝。
    pub fn connect(&self, protocol: &[u8]) -> Result<Channel, Status> {
        let (client, server) = Channel::create()?;
        self.forward(protocol, server)?;
        Ok(client)
    }

    /// 把一条已有连接的服务端转交给 `protocol` 的提供者（路由者转发消费者的请求时使用）。
    ///
    /// # Errors
    ///
    /// 同 [`Self::connect`]；失败时 `server` 随请求关闭，消费者的一端因此 `PEER_CLOSED`。
    pub fn forward(&self, protocol: &[u8], server: Channel) -> Result<(), Status> {
        let mut request = Writer::request(CONNECT);
        request.bytes(protocol);
        call(&self.0, &request, alloc::vec![server.into_handle()]).map(drop)
    }

    /// 底层 channel（等待提供者终止时使用）。
    pub fn channel(&self) -> &Channel {
        &self.0
    }
}
