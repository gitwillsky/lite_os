#![no_std]
//! 网络设置（netcfg 的 `liteos.network.Settings`）与 DHCPv4 租约（dhcp-client 的 `liteos.network.Dhcp`）的编码。
//!
//! 1. 只定义数据与字节编码，不依赖原生运行时：原生服务以 `service_rpc` 传输，Linux 程序经 linuxd 的
//!    `/dev/network-settings` datagram 适配（[`bridge`] 帧格式）传输同一编码。
//! 2. 整数为 little endian；变长字段以 `u32` 长度前缀；解码拒绝越界、未知枚举值与多余字节。

extern crate alloc;

use alloc::vec::Vec;
use core::net::Ipv4Addr;

/// 每个接口最多保存的 DNS 服务器数（与 musl 的 `MAXNS` 一致）。
pub const DNS_MAX: usize = 3;
/// 接口名上限（含结尾 NUL 的 `IFNAMSIZ`）。
pub const NAME_MAX: usize = 16;

/// 设置协议的请求序号。
pub mod ordinal {
    /// 取当前快照（[`super::Snapshot`]）。
    pub const GET: u32 = 1;
    /// 设置一个接口（名称 + [`super::InterfaceConfig`]），持久化后立即应用。
    pub const SET: u32 = 2;
    /// 等待代际超过给定值（`u64`）；回复新的代际。
    pub const WAIT: u32 = 3;
}

/// 解码失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// 接口的配置方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// 接口关闭，不配置地址。
    Disabled = 0,
    /// 经 dhcp-client 取得地址、网关与 DNS。
    Dhcp = 1,
    /// 使用配置中的地址、前缀、网关与 DNS。
    Static = 2,
}

/// 一个接口的用户设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceConfig {
    pub method: Method,
    pub address: Ipv4Addr,
    pub prefix_length: u8,
    /// 默认网关；`UNSPECIFIED` 表示没有。
    pub gateway: Ipv4Addr,
    /// 静态 DNS；DHCP 方法下非空时覆盖租约中的 DNS（Windows“使用下面的 DNS 服务器地址”）。
    pub dns: Vec<Ipv4Addr>,
}

impl InterfaceConfig {
    /// 未单独设置的接口沿用的默认值：DHCP。
    pub fn dhcp() -> Self {
        Self {
            method: Method::Dhcp,
            address: Ipv4Addr::UNSPECIFIED,
            prefix_length: 0,
            gateway: Ipv4Addr::UNSPECIFIED,
            dns: Vec::new(),
        }
    }
}

/// 一个接口的当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceState {
    /// 接口名（`eth0` 等）。
    pub name: Vec<u8>,
    pub mac: [u8; 6],
    pub config: InterfaceConfig,
    /// 已配置的地址；未取得时为 `UNSPECIFIED`。
    pub address: Ipv4Addr,
    pub prefix_length: u8,
    pub gateway: Ipv4Addr,
    /// 正在生效的 DNS。
    pub dns: Vec<Ipv4Addr>,
    /// 当前地址配置生效以来的秒数（“状态”里的持续时间）；没有地址时为零。
    pub connected_seconds: u32,
}

/// 设置快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 每次状态变化递增；`WAIT` 以它判断是否有变化。
    pub generation: u64,
    pub interfaces: Vec<InterfaceState>,
}

impl Snapshot {
    /// 全部接口正在生效的 DNS（按接口顺序去重，至多 [`DNS_MAX`] 个），即 `/etc/resolv.conf` 的内容来源。
    pub fn nameservers(&self) -> Vec<Ipv4Addr> {
        let mut servers = Vec::new();
        for server in self.interfaces.iter().flat_map(|interface| &interface.dns) {
            if servers.len() < DNS_MAX && !servers.contains(server) {
                servers.push(*server);
            }
        }
        servers
    }
}

/// DHCPv4 租约（dhcp-client → netcfg 的事件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseEvent {
    /// 取得或续租成功。
    Bound {
        address: Ipv4Addr,
        prefix_length: u8,
        gateway: Ipv4Addr,
        dns: Vec<Ipv4Addr>,
        /// 租期（秒）。
        lease_seconds: u32,
    },
    /// 租约失效（到期、被拒绝或链路丢失）：撤销地址后重新获取。
    Lost,
}

/// 编码器。
#[derive(Default)]
pub struct Encoder(pub Vec<u8>);

impl Encoder {
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn address(&mut self, value: Ipv4Addr) -> &mut Self {
        self.0.extend_from_slice(&value.octets());
        self
    }
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.u32(value.len() as u32);
        self.0.extend_from_slice(value);
        self
    }
    fn addresses(&mut self, values: &[Ipv4Addr]) -> &mut Self {
        self.u8(values.len().min(DNS_MAX) as u8);
        for value in values.iter().take(DNS_MAX) {
            self.address(*value);
        }
        self
    }
    pub fn config(&mut self, config: &InterfaceConfig) -> &mut Self {
        self.u8(config.method as u8)
            .address(config.address)
            .u8(config.prefix_length)
            .address(config.gateway)
            .addresses(&config.dns)
    }
    pub fn snapshot(&mut self, snapshot: &Snapshot) -> &mut Self {
        self.u64(snapshot.generation)
            .u32(snapshot.interfaces.len() as u32);
        for interface in &snapshot.interfaces {
            self.bytes(&interface.name);
            self.0.extend_from_slice(&interface.mac);
            self.config(&interface.config)
                .address(interface.address)
                .u8(interface.prefix_length)
                .address(interface.gateway)
                .addresses(&interface.dns)
                .u32(interface.connected_seconds);
        }
        self
    }
    pub fn lease(&mut self, event: &LeaseEvent) -> &mut Self {
        match event {
            LeaseEvent::Bound {
                address,
                prefix_length,
                gateway,
                dns,
                lease_seconds,
            } => self
                .u8(1)
                .address(*address)
                .u8(*prefix_length)
                .address(*gateway)
                .addresses(dns)
                .u32(*lease_seconds),
            LeaseEvent::Lost => self.u8(0),
        }
    }
}

/// 解码器。
pub struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], Invalid> {
        if self.0.len() < count {
            return Err(Invalid);
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }
    /// 输入必须恰好用尽。
    pub fn finish(&self) -> Result<(), Invalid> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Invalid)
        }
    }
    pub fn u8(&mut self) -> Result<u8, Invalid> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, Invalid> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| Invalid)?,
        ))
    }
    pub fn u64(&mut self) -> Result<u64, Invalid> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Invalid)?,
        ))
    }
    pub fn address(&mut self) -> Result<Ipv4Addr, Invalid> {
        let octets: [u8; 4] = self.take(4)?.try_into().map_err(|_| Invalid)?;
        Ok(Ipv4Addr::from(octets))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], Invalid> {
        let length = self.u32()? as usize;
        self.take(length)
    }
    fn addresses(&mut self) -> Result<Vec<Ipv4Addr>, Invalid> {
        let count = usize::from(self.u8()?);
        if count > DNS_MAX {
            return Err(Invalid);
        }
        (0..count).map(|_| self.address()).collect()
    }
    fn prefix(&mut self) -> Result<u8, Invalid> {
        let prefix = self.u8()?;
        if prefix > 32 {
            Err(Invalid)
        } else {
            Ok(prefix)
        }
    }
    pub fn config(&mut self) -> Result<InterfaceConfig, Invalid> {
        let method = match self.u8()? {
            0 => Method::Disabled,
            1 => Method::Dhcp,
            2 => Method::Static,
            _ => return Err(Invalid),
        };
        Ok(InterfaceConfig {
            method,
            address: self.address()?,
            prefix_length: self.prefix()?,
            gateway: self.address()?,
            dns: self.addresses()?,
        })
    }
    pub fn snapshot(&mut self) -> Result<Snapshot, Invalid> {
        let generation = self.u64()?;
        let count = self.u32()? as usize;
        // 每个接口至少 34 字节；先按剩余输入约束数量，畸形输入不能引发大分配。
        if count > self.0.len() / 34 {
            return Err(Invalid);
        }
        let mut interfaces = Vec::with_capacity(count);
        for _ in 0..count {
            let name = self.bytes()?;
            if name.is_empty() || name.len() >= NAME_MAX {
                return Err(Invalid);
            }
            let name = name.to_vec();
            let mac: [u8; 6] = self.take(6)?.try_into().map_err(|_| Invalid)?;
            interfaces.push(InterfaceState {
                name,
                mac,
                config: self.config()?,
                address: self.address()?,
                prefix_length: self.prefix()?,
                gateway: self.address()?,
                dns: self.addresses()?,
                connected_seconds: self.u32()?,
            });
        }
        Ok(Snapshot {
            generation,
            interfaces,
        })
    }
    pub fn lease(&mut self) -> Result<LeaseEvent, Invalid> {
        match self.u8()? {
            0 => Ok(LeaseEvent::Lost),
            1 => Ok(LeaseEvent::Bound {
                address: self.address()?,
                prefix_length: self.prefix()?,
                gateway: self.address()?,
                dns: self.addresses()?,
                lease_seconds: self.u32()?,
            }),
            _ => Err(Invalid),
        }
    }
}

/// linuxd 把设置协议适配为 Linux datagram 时的帧格式。
pub mod bridge {
    /// 服务端点（root:netdev 0660 的 pathname datagram socket）。
    pub const PATH: &[u8] = b"/dev/network-settings";
    /// 允许修改网络设置的组（`netdev`）。
    pub const GROUP: u32 = 28;
    /// 请求：`u32` 序号后接载荷；回复：`i32` 状态（0 成功，否则原生状态码）后接载荷。
    pub const HEADER_BYTES: usize = 4;
    /// 单个 datagram 的上限：请求与回复都必须装下。
    pub const DATAGRAM_MAX: usize = 16 * 1024;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn snapshot_round_trips_and_rejects_trailing_or_out_of_range_fields() {
        let snapshot = Snapshot {
            generation: 7,
            interfaces: vec![InterfaceState {
                name: b"eth0".to_vec(),
                mac: [2, 0, 0, 0, 0, 1],
                config: InterfaceConfig {
                    method: Method::Static,
                    address: Ipv4Addr::new(10, 0, 2, 15),
                    prefix_length: 24,
                    gateway: Ipv4Addr::new(10, 0, 2, 2),
                    dns: vec![Ipv4Addr::new(10, 0, 2, 3)],
                },
                address: Ipv4Addr::new(10, 0, 2, 15),
                prefix_length: 24,
                gateway: Ipv4Addr::new(10, 0, 2, 2),
                dns: vec![Ipv4Addr::new(10, 0, 2, 3)],
                connected_seconds: 42,
            }],
        };
        let mut encoder = Encoder::default();
        encoder.snapshot(&snapshot);
        let mut decoder = Decoder::new(&encoder.0);
        assert_eq!(decoder.snapshot(), Ok(snapshot));
        assert_eq!(decoder.finish(), Ok(()));
        let mut bytes = encoder.0.clone();
        bytes.push(0);
        let mut decoder = Decoder::new(&bytes);
        assert!(decoder.snapshot().is_ok());
        assert_eq!(decoder.finish(), Err(Invalid));
        // 前缀长度位于名称、MAC、方法与地址之后。
        let prefix = 8 + 4 + 4 + 4 + 6 + 1 + 4;
        let mut bytes = encoder.0;
        bytes[prefix] = 33;
        assert_eq!(Decoder::new(&bytes).snapshot(), Err(Invalid));
    }
}
