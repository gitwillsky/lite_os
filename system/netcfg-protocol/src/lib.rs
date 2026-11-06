#![no_std]
//! 网络设置（netcfg 的 `liteos.network.Settings`）与 DHCPv4 租约（dhcp-client 的 `liteos.network.Dhcp`）的编码。
//!
//! 1. 只定义数据与字节编码，不依赖原生运行时：原生服务以 `service_rpc` 传输，Linux 程序经 linuxd 的
//!    `/dev/network-settings` datagram 适配（[`bridge`] 帧格式）传输同一编码。
//! 2. 整数为 little endian；变长字段以 `u32` 长度前缀；解码拒绝越界、未知枚举值与多余字节。

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::net::Ipv4Addr;

/// 每个接口最多保存的 DNS 服务器数（与 musl 的 `MAXNS` 一致）。
pub const DNS_MAX: usize = 3;
/// 接口名上限（含结尾 NUL 的 `IFNAMSIZ`）。
pub const NAME_MAX: usize = 16;
/// 连接显示名的 UTF-8 字节上限。
pub const CONNECTION_NAME_MAX: usize = 255;
/// 可写入 musl 的 256 字节 resolv.conf 行缓冲的单个搜索域长度。
pub const DNS_SUFFIX_MAX: usize = 247;

/// 判断有序后缀能否完整写入解析器配置。空列表使用连接后缀。
pub fn valid_search_list(list: &[String]) -> bool {
    let mut length = 0;
    for (index, suffix) in list.iter().enumerate() {
        if suffix.is_empty()
            || dns_suffix(suffix.as_bytes()).is_err()
            || list[..index]
                .iter()
                .any(|previous| previous.eq_ignore_ascii_case(suffix))
        {
            return false;
        }
        length += suffix.len() + usize::from(index != 0);
        if length > DNS_SUFFIX_MAX {
            return false;
        }
    }
    true
}

/// 设置协议的请求序号。
pub mod ordinal {
    /// 取当前快照（[`super::Snapshot`]）。
    pub const GET: u32 = 1;
    /// 原子更新接口配置或全局搜索列表（[`super::Update`]）。
    pub const SET: u32 = 2;
    /// 等待代际超过给定值（`u64`）；回复新的代际。
    pub const WAIT: u32 = 3;
    /// 操作指定接口的 DHCP 租约（名称 + [`super::DhcpAction`]）。
    pub const DHCP: u32 = 4;
    /// 重命名连接（接口名 + UTF-8 显示名）；只修改连接名称。
    pub const RENAME: u32 = 5;
}

/// 有权限的用户请求 DHCP 会话立即续租或释放。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DhcpAction {
    Renew = 1,
    Release = 2,
}

impl DhcpAction {
    /// 从设置请求或会话命令解码；未知操作返回 `Invalid`。
    pub fn decode(value: u8) -> Result<Self, Invalid> {
        match value {
            1 => Ok(Self::Renew),
            2 => Ok(Self::Release),
            _ => Err(Invalid),
        }
    }
}

/// 解码失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// 验证 DNS 搜索后缀。空字符串表示无手动设置或未获租约后缀。
///
/// # Errors
///
/// 非 ASCII DNS 主机名、超长标签或超长后缀。国际化域名使用 ASCII A-label。
pub fn dns_suffix(bytes: &[u8]) -> Result<&str, Invalid> {
    let suffix = core::str::from_utf8(bytes).map_err(|_| Invalid)?;
    if !suffix.is_empty()
        && (suffix.len() > DNS_SUFFIX_MAX
            || suffix.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }))
    {
        return Err(Invalid);
    }
    Ok(suffix)
}

/// 验证连接显示名的编码、长度与 netshell 字符约束。
///
/// # Errors
///
/// 无效 UTF-8、空白名称、控制字符、保留字符或超长名称。
pub fn connection_name(bytes: &[u8]) -> Result<&str, Invalid> {
    let name = core::str::from_utf8(bytes).map_err(|_| Invalid)?;
    if name.len() > CONNECTION_NAME_MAX
        || name.trim().is_empty()
        || name
            .chars()
            .any(|ch| ch.is_control() || "\\/:\"*?<>|".contains(ch))
    {
        return Err(Invalid);
    }
    Ok(name)
}

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
    /// 手动 DNS 后缀，空字符串使用 DHCP 的后缀。
    pub dns_suffix: alloc::string::String,
}

/// 一次设置提交。缺省的字段保留提交时的系统值。
#[derive(Default)]
pub struct Update<'a> {
    /// `config` 的目标接口名。只更新搜索列表时为空。
    pub interface: &'a [u8],
    /// 可选的接口配置替换。
    pub config: Option<InterfaceConfig>,
    /// 可选的全局列表替换，空列表恢复自动搜索。
    pub search_list: Option<Vec<String>>,
    /// 可选的主 DNS 后缀替换，空字符串清除后缀。
    pub primary_dns_suffix: Option<String>,
    /// 可选的自动搜索父后缀开关。
    pub append_parent_suffixes: Option<bool>,
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
            dns_suffix: alloc::string::String::new(),
        }
    }
}

/// 一个接口的当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceState {
    /// 接口名（`eth0` 等）。
    pub name: Vec<u8>,
    /// 用户指定的连接显示名。空字节串表示使用桌面默认名称。
    pub connection_name: Vec<u8>,
    pub mac: [u8; 6],
    pub config: InterfaceConfig,
    /// 已配置的地址；未取得时为 `UNSPECIFIED`。
    pub address: Ipv4Addr,
    pub prefix_length: u8,
    pub gateway: Ipv4Addr,
    /// 正在生效的 DNS。
    pub dns: Vec<Ipv4Addr>,
    /// 正在生效的连接特定 DNS 后缀。
    pub dns_suffix: alloc::string::String,
    /// 当前地址配置生效以来的秒数（“状态”里的持续时间）；没有地址时为零。
    pub connected_seconds: u32,
    /// 当前生效的 DHCP 租约；静态、禁用和等待地址时为空。
    pub lease: Option<LeaseStatus>,
}

/// 已生效租约的只读状态。时刻是 Unix UTC 秒，由配置服务从当前时钟换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseStatus {
    /// DHCP Server Identifier（选项 54）。
    pub server: Ipv4Addr,
    /// 取得这次租约的请求时刻。
    pub obtained_at: i64,
    /// 租约过期时刻；无限租期为 `None`。
    pub expires_at: Option<i64>,
}

/// 从 DHCPACK 建立的租约计时事实；续租和过期使用同一单调时钟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTiming {
    /// DHCP Server Identifier（选项 54）。
    pub server: Ipv4Addr,
    /// DHCPREQUEST 发出的单调时钟纳秒。
    pub requested_at_ns: i64,
    /// 服务器授予的秒数；`u32::MAX` 表示无限租期。
    pub duration_seconds: u32,
}

/// 设置快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// 每次状态变化递增；`WAIT` 以它判断是否有变化。
    pub generation: u64,
    pub interfaces: Vec<InterfaceState>,
    /// 手动搜索列表，空列表使用各连接的生效后缀。
    pub search_list: Vec<String>,
    /// 计算机的主 DNS 后缀。空字符串表示未设置。
    pub primary_dns_suffix: String,
    /// 自动搜索时是否尝试主 DNS 后缀的父级。
    pub append_parent_suffixes: bool,
}

impl Snapshot {
    /// 当前解析器搜索顺序。手动列表覆盖各接口后缀。
    pub fn search_domains(&self) -> Vec<&str> {
        if !self.search_list.is_empty() {
            return self.search_list.iter().map(String::as_str).collect();
        }
        let mut domains: Vec<&str> = Vec::new();
        let mut length = 0;
        // Windows 2000 先尝试主后缀及连接后缀，再逐级缩短主后缀，保留至少两个标签。
        let primary = self.primary_dns_suffix.as_str();
        let parents =
            core::iter::successors(self.append_parent_suffixes.then_some(primary), |suffix| {
                suffix
                    .split_once('.')
                    .map(|(_, parent)| parent)
                    .filter(|parent| parent.contains('.'))
            })
            .skip(1);
        for suffix in core::iter::once(primary)
            .chain(
                self.interfaces
                    .iter()
                    .map(|entry| entry.dns_suffix.as_str()),
            )
            .chain(parents)
        {
            let size = suffix.len() + usize::from(!domains.is_empty());
            if !suffix.is_empty()
                && length + size <= DNS_SUFFIX_MAX
                && !domains
                    .iter()
                    .any(|saved| saved.eq_ignore_ascii_case(suffix))
            {
                domains.push(suffix);
                length += size;
            }
        }
        domains
    }

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
        dns_suffix: alloc::string::String,
        timing: LeaseTiming,
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
    fn i64(&mut self, value: i64) -> &mut Self {
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
            .bytes(config.dns_suffix.as_bytes())
    }
    /// 写入已经过 `valid_search_list` 验证的有序列表，返回当前编码器。
    pub fn search_list(&mut self, list: &[String]) -> &mut Self {
        self.u8(list.len() as u8);
        for suffix in list {
            self.bytes(suffix.as_bytes());
        }
        self
    }
    /// 写入一次配置提交及字段存在标记，返回当前编码器。
    pub fn update(&mut self, update: &Update<'_>) -> &mut Self {
        self.bytes(update.interface)
            .u8(u8::from(update.config.is_some()));
        if let Some(config) = &update.config {
            self.config(config);
        }
        self.u8(u8::from(update.search_list.is_some()));
        if let Some(search) = &update.search_list {
            self.search_list(search);
        }
        self.u8(u8::from(update.primary_dns_suffix.is_some()));
        if let Some(suffix) = &update.primary_dns_suffix {
            self.bytes(suffix.as_bytes());
        }
        self.u8(update
            .append_parent_suffixes
            .map_or(0, |enabled| 1 + u8::from(enabled)))
    }
    pub fn snapshot(&mut self, snapshot: &Snapshot) -> &mut Self {
        self.u64(snapshot.generation)
            .u32(snapshot.interfaces.len() as u32);
        for interface in &snapshot.interfaces {
            self.bytes(&interface.name)
                .bytes(&interface.connection_name);
            self.0.extend_from_slice(&interface.mac);
            self.config(&interface.config)
                .address(interface.address)
                .u8(interface.prefix_length)
                .address(interface.gateway)
                .addresses(&interface.dns)
                .bytes(interface.dns_suffix.as_bytes())
                .u32(interface.connected_seconds);
            match interface.lease {
                None => self.u8(0),
                Some(lease) => {
                    self.u8(1).address(lease.server).i64(lease.obtained_at);
                    match lease.expires_at {
                        None => self.u8(0),
                        Some(expires_at) => self.u8(1).i64(expires_at),
                    }
                }
            };
        }
        self.search_list(&snapshot.search_list)
            .bytes(snapshot.primary_dns_suffix.as_bytes())
            .u8(u8::from(snapshot.append_parent_suffixes))
    }
    pub fn lease(&mut self, event: &LeaseEvent) -> &mut Self {
        match event {
            LeaseEvent::Bound {
                address,
                prefix_length,
                gateway,
                dns,
                dns_suffix,
                timing,
            } => self
                .u8(1)
                .address(*address)
                .u8(*prefix_length)
                .address(*gateway)
                .addresses(dns)
                .bytes(dns_suffix.as_bytes())
                .address(timing.server)
                .i64(timing.requested_at_ns)
                .u32(timing.duration_seconds),
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
    fn i64(&mut self) -> Result<i64, Invalid> {
        Ok(i64::from_le_bytes(
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
    /// 解码配置。旧磁盘格式（版本 1、2）以 `with_suffix = false` 读取。
    ///
    /// # Errors
    ///
    /// 字段截断或值无效。
    pub fn config(&mut self, with_suffix: bool) -> Result<InterfaceConfig, Invalid> {
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
            dns_suffix: if with_suffix {
                dns_suffix(self.bytes()?)?.into()
            } else {
                Default::default()
            },
        })
    }
    /// 读取全局搜索列表，按提交顺序返回。
    ///
    /// # Errors
    ///
    /// 输入截断、域名重复或列表超出解析器上限。
    pub fn search_list(&mut self) -> Result<Vec<String>, Invalid> {
        let count = usize::from(self.u8()?);
        if count > self.0.len() / 5 || count > DNS_SUFFIX_MAX.div_ceil(2) {
            return Err(Invalid);
        }
        let mut list = Vec::with_capacity(count);
        for _ in 0..count {
            list.push(dns_suffix(self.bytes()?)?.into());
        }
        valid_search_list(&list).then_some(list).ok_or(Invalid)
    }
    /// 解码一笔提交，保留未提供的字段为 `None`。
    ///
    /// # Errors
    ///
    /// 输入截断、字段无效或没有任何提交内容。
    pub fn update(&mut self) -> Result<Update<'a>, Invalid> {
        let interface = self.bytes()?;
        let config = match self.u8()? {
            0 => None,
            1 => Some(self.config(true)?),
            _ => return Err(Invalid),
        };
        let search_list = match self.u8()? {
            0 => None,
            1 => Some(self.search_list()?),
            _ => return Err(Invalid),
        };
        let primary_dns_suffix = match self.u8()? {
            0 => None,
            1 => Some(dns_suffix(self.bytes()?)?.into()),
            _ => return Err(Invalid),
        };
        let append_parent_suffixes = match self.u8()? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            _ => return Err(Invalid),
        };
        if config.is_none()
            && search_list.is_none()
            && primary_dns_suffix.is_none()
            && append_parent_suffixes.is_none()
        {
            return Err(Invalid);
        }
        Ok(Update {
            interface,
            config,
            search_list,
            primary_dns_suffix,
            append_parent_suffixes,
        })
    }
    pub fn snapshot(&mut self) -> Result<Snapshot, Invalid> {
        let generation = self.u64()?;
        let count = self.u32()? as usize;
        // 按每个接口的固定字段下限约束数量，畸形输入不能引发大分配。
        if count > self.0.len() / 47 {
            return Err(Invalid);
        }
        let mut interfaces = Vec::with_capacity(count);
        for _ in 0..count {
            let name = self.bytes()?;
            if name.is_empty() || name.len() >= NAME_MAX {
                return Err(Invalid);
            }
            let name = name.to_vec();
            let connection_name = self.bytes()?;
            if !connection_name.is_empty() {
                self::connection_name(connection_name)?;
            }
            let mac: [u8; 6] = self.take(6)?.try_into().map_err(|_| Invalid)?;
            interfaces.push(InterfaceState {
                name,
                connection_name: connection_name.to_vec(),
                mac,
                config: self.config(true)?,
                address: self.address()?,
                prefix_length: self.prefix()?,
                gateway: self.address()?,
                dns: self.addresses()?,
                dns_suffix: dns_suffix(self.bytes()?)?.into(),
                connected_seconds: self.u32()?,
                lease: match self.u8()? {
                    0 => None,
                    1 => Some(LeaseStatus {
                        server: self.address()?,
                        obtained_at: self.i64()?,
                        expires_at: match self.u8()? {
                            0 => None,
                            1 => Some(self.i64()?),
                            _ => return Err(Invalid),
                        },
                    }),
                    _ => return Err(Invalid),
                },
            });
        }
        Ok(Snapshot {
            generation,
            interfaces,
            search_list: self.search_list()?,
            primary_dns_suffix: dns_suffix(self.bytes()?)?.into(),
            append_parent_suffixes: match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(Invalid),
            },
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
                dns_suffix: dns_suffix(self.bytes()?)?.into(),
                timing: LeaseTiming {
                    server: self.address()?,
                    requested_at_ns: self.i64()?,
                    duration_seconds: self.u32()?,
                },
            }),
            _ => Err(Invalid),
        }
    }
}

/// linuxd 把设置协议适配为 Linux datagram 时的帧格式。
pub mod bridge {
    /// 修改设置与订阅代际端点（root:netdev 0660）。
    pub const PATH: &[u8] = b"/dev/network-settings";
    /// 只读状态端点，普通用户可查询当前地址与 DNS。
    pub const STATUS_PATH: &[u8] = b"/dev/network-status";
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
            search_list: vec!["one.test".into(), "two.test".into()],
            primary_dns_suffix: "sub.office.test".into(),
            append_parent_suffixes: true,
            interfaces: vec![InterfaceState {
                name: b"eth0".to_vec(),
                connection_name: "办公网".as_bytes().to_vec(),
                mac: [2, 0, 0, 0, 0, 1],
                config: InterfaceConfig::dhcp(),
                address: Ipv4Addr::new(10, 0, 2, 15),
                prefix_length: 24,
                gateway: Ipv4Addr::new(10, 0, 2, 2),
                dns: vec![Ipv4Addr::new(10, 0, 2, 3)],
                dns_suffix: "office.test".into(),
                connected_seconds: 42,
                lease: Some(LeaseStatus {
                    server: Ipv4Addr::new(10, 0, 2, 2),
                    obtained_at: 1_700_000_000,
                    expires_at: Some(1_700_000_600),
                }),
            }],
        };
        let mut encoder = Encoder::default();
        encoder.snapshot(&snapshot);
        let mut decoder = Decoder::new(&encoder.0);
        assert_eq!(decoder.snapshot(), Ok(snapshot));
        assert_eq!(decoder.finish(), Ok(()));
        assert_eq!(
            Decoder::new(&encoder.0[..encoder.0.len() - 1]).snapshot(),
            Err(Invalid)
        );
        let mut bytes = encoder.0.clone();
        bytes.push(0);
        let mut decoder = Decoder::new(&bytes);
        assert!(decoder.snapshot().is_ok());
        assert_eq!(decoder.finish(), Err(Invalid));
        // 前缀长度位于两个名称、MAC、方法与地址之后。
        let prefix = 8 + 4 + 4 + 4 + 4 + "办公网".len() + 6 + 1 + 4;
        let mut bytes = encoder.0;
        bytes[prefix] = 33;
        assert_eq!(Decoder::new(&bytes).snapshot(), Err(Invalid));
    }
}
