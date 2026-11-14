#![no_std]
#![feature(allocator_ext)]
//! router-config 原生配置模型/元数据、设置投影（`liteos.network.Settings`）、DHCPv4 租约与路由来源编码。
//!
//! 1. 只定义数据与字节编码，不依赖原生运行时：原生服务以 `service_rpc` 传输，Linux 程序经 linuxd 的
//!    `/dev/network-settings` datagram 适配（[`bridge`] 帧格式）传输同一编码。
//! 2. 整数为 little endian；变长字段以 `u32` 长度前缀；解码拒绝越界、未知枚举值与多余字节。
//! 3. 原生配置的 JSON/语义校验/字段元数据归 [`configuration`]，不依赖 Linux 接口身份或 libc 容量。
//! 4. [`routing`] 定义双栈候选声明；来源授权、递归求解和 FIB 执行不属于格式层。

extern crate alloc;

pub mod administration;
pub mod backup;
pub mod bgp;
pub mod configuration;
pub mod connections;
pub mod delegation;
pub mod dhcp_server;
pub mod dns;
pub mod dns_cache;
pub mod firewall;
pub mod interface;
pub mod multicast;
pub mod neighbors;
pub mod ospf;
pub mod policy;
pub mod policy_routing;
mod revision;
pub mod route_view;
pub mod routing;
mod secret;
pub mod switching;
pub mod tracking;

/// 设置/租约消息的固定线版本；解码不接受旧布局或未知版本。
pub const VERSION: u32 = 5;

pub use revision::Revision;

use alloc::{collections::BTreeSet, string::String, vec::Vec};
use core::net::{Ipv4Addr, SocketAddr};

/// 接口名上限（含结尾 NUL 的 `IFNAMSIZ`）。
pub const NAME_MAX: usize = 16;
/// 连接显示名的 UTF-8 字节上限。
pub const CONNECTION_NAME_MAX: usize = 255;
/// DNS 文本名字上限（不含结尾根点）；解析器自身的容量由各投影消费者处理。
pub const DNS_SUFFIX_MAX: usize = 253;

/// 验证有序原生搜索域，空列表使用连接后缀；不引入某一解析器的行缓冲限制。
pub fn valid_search_list(list: &[String]) -> bool {
    let mut suffixes = BTreeSet::new();
    for suffix in list {
        if suffix.is_empty()
            || dns_suffix(suffix.as_bytes()).is_err()
            || !suffixes.insert(suffix.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}

/// 设置协议的请求序号。
pub mod ordinal {
    /// 是否使用带独立文档能力的原生配置管理格式。
    pub fn is_configuration(operation: u32) -> bool {
        matches!(
            operation,
            EXPORT
                | SCHEMA
                | VALIDATE
                | IMPORT
                | DIFF
                | BACKUPS
                | BACKUP_CREATE
                | BACKUP_EXPORT
                | BACKUP_DELETE
                | BGP_STATUS
                | OSPF_STATUS
                | OPEN_NETWORK_NAMESPACE
                | ROUTING_DOMAINS
                | BGP_ROUTES
                | BGP_REFRESH
                | DHCP_SERVER_BINDINGS
                | CONNECTIONS
                | CLEAR_CONNECTION
                | CONNECTION_CHANGES
                | FIREWALL_COUNTERS
                | NEIGHBOR_PAGE
                | ROUTE_PAGE
                | COMMIT_RESULT
        )
    }
    /// 取当前快照（[`super::Snapshot`]）。
    pub const GET: u32 = 1;
    /// 原子更新接口配置或全局搜索列表（[`super::Update`]）。
    pub const SET: u32 = 2;
    /// 等待设置版本变化。回复当前版本，有界等待到期时可能与请求相同。
    pub const WAIT: u32 = 3;
    /// 操作指定接口的 DHCP 租约（名称 + [`super::DhcpAction`]）。
    pub const DHCP: u32 = 4;
    /// 重命名连接（接口名 + UTF-8 显示名）；只修改连接名称。
    pub const RENAME: u32 = 5;
    /// 导出当前原生配置；控制消息与独立内容能力的格式见 administration。
    pub const EXPORT: u32 = 6;
    /// 查询原生配置字段的 JSON Schema；请求/回复外壳同 EXPORT。
    pub const SCHEMA: u32 = 7;
    /// 无副作用校验候选 JSON 和当前设备匹配；成功回复规范化配置 JSON。
    pub const VALIDATE: u32 = 8;
    /// 按预期快照代际完整替换配置；成功只回复新的代际，没有文档能力。
    pub const IMPORT: u32 = 9;
    /// 对比当前配置与候选 JSON；回复 JSON Pointer 路径及变更前后值。
    pub const DIFF: u32 = 10;
    /// 查询备份目录和自动任务状态。
    pub const BACKUPS: u32 = 11;
    /// 立即创建一个不受自动保留策略删除的手动备份。
    pub const BACKUP_CREATE: u32 = 12;
    /// 按固定备份身份导出其配置 JSON。
    pub const BACKUP_EXPORT: u32 = 13;
    /// 显式删除一个备份并同步目录。
    pub const BACKUP_DELETE: u32 = 14;
    /// 当前 BGP 邻居会话状态；只读观察，不把接收路由数当作 FIB 安装数量。
    pub const BGP_STATUS: u32 = 15;
    /// 按规范前缀查询 BGP 原始候选、确认选择及未采用原因，输入/输出经文档能力传递。
    pub const BGP_ROUTES: u32 = 16;
    /// Query one page of active downstream DHCPv4 leases on a selected interface.
    pub const DHCP_SERVER_BINDINGS: u32 = 17;
    /// 为已建立的 BGP 邻居排队单族刷新，不持久化、不重连。
    pub const BGP_REFRESH: u32 = 18;
    /// 读取当前网络域的连接页。
    pub const CONNECTIONS: u32 = 19;
    /// 清除指定的活动转发连接。
    pub const CLEAR_CONNECTION: u32 = 20;
    /// 读取下一批连接增量，或取得重新同步起点。
    pub const CONNECTION_CHANGES: u32 = 21;
    /// 等待指定连接增量游标变为可读。
    pub const WAIT_CONNECTION_CHANGES: u32 = 22;
    /// 读取当前预路由规则的命中计数。
    pub const FIREWALL_COUNTERS: u32 = 23;
    /// 按提交身份查询已持久化的结果。
    pub const COMMIT_RESULT: u32 = 24;
    /// 按逻辑接口与地址读取一页双栈邻居。
    pub const NEIGHBOR_PAGE: u32 = 25;
    /// 按地址族和位置读取一页生效路由及下一跳。
    pub const ROUTE_PAGE: u32 = 26;
    /// 查询原生 OSPF 发现状态。
    pub const OSPF_STATUS: u32 = 27;
    /// 取得已配置具名域的真实网络命名空间能力。只允许本地特权设置端点调用。
    pub const OPEN_NETWORK_NAMESPACE: u32 = 28;
    /// 查询已保存的具名路由域名称，供获授权的观察者选择。
    pub const ROUTING_DOMAINS: u32 = 29;
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
    /// 校验当前接口配置投影的字段约束；所有提交、磁盘读取和协议解码共用此 owner。
    /// # Errors
    /// 前缀/后缀不可表达，或静态配置缺少地址。
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.prefix_length > 32 || self.method == Method::Static && self.address.is_unspecified()
        {
            return Err(Invalid);
        }
        dns_suffix(self.dns_suffix.as_bytes())?;
        Ok(())
    }
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

impl Update<'_> {
    /// 校验一笔配置修改的全部字段；本地构造不能绕过与 wire 请求相同的校验。
    /// # Errors
    /// 无提交内容、接口目标无效、接口配置/搜索列表/主后缀非法。
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.config.is_none()
            && self.search_list.is_none()
            && self.primary_dns_suffix.is_none()
            && self.append_parent_suffixes.is_none()
        {
            return Err(Invalid);
        }
        if let Some(config) = &self.config {
            if self.interface.is_empty()
                || self.interface.len() >= NAME_MAX
                || self.interface.contains(&0)
            {
                return Err(Invalid);
            }
            config.validate()?;
        }
        if self
            .search_list
            .as_ref()
            .is_some_and(|list| !valid_search_list(list))
        {
            return Err(Invalid);
        }
        if let Some(suffix) = &self.primary_dns_suffix {
            dns_suffix(suffix.as_bytes())?;
        }
        Ok(())
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
    /// 每次发布状态变化更新；`WAIT` 和 `IMPORT` 使用同一代际。
    pub generation: Revision,
    /// 配置 owner 的提交进度；非 Ready 时不能把缓存的运行时事实视为最终已确认状态。
    pub configuration_status: ConfigurationStatus,
    pub interfaces: Vec<InterfaceState>,
    /// 手动搜索列表，空列表使用各连接的生效后缀。
    pub search_list: Vec<String>,
    /// 计算机的主 DNS 后缀。空字符串表示未设置。
    pub primary_dns_suffix: String,
    /// 自动搜索时是否尝试主 DNS 后缀的父级。
    pub append_parent_suffixes: bool,
    /// 当前有效的路由通告 DNS，属于运行时设置，不写入配置树。
    pub learned_dns: dns::LearnedDns,
}

/// 已提交意图和执行/持久化协调的状态，由 owner 的实际待处理工作投影。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConfigurationStatus {
    /// 没有待撤销的文件或待重新应用的接口。
    Ready = 0,
    /// 文件替换失败且原文件尚未确认恢复；新的持久设置提交暂停。
    RollingBack = 1,
    /// 持久设置已确定，执行确认或接口应用仍需重试。
    Applying = 2,
    /// 持久配置加载失败；保留管理查询、禁用接口并禁止局部提交覆盖未知配置。
    Rejected = 3,
}

impl Snapshot {
    /// 当前解析器搜索顺序。手动列表覆盖各接口后缀。
    pub fn search_domains(&self) -> Vec<&str> {
        if !self.search_list.is_empty() {
            return self.search_list.iter().map(String::as_str).collect();
        }
        let mut domains: Vec<&str> = Vec::new();
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
            .chain(self.learned_dns.domains.iter().map(String::as_str))
            .chain(parents)
        {
            if !suffix.is_empty()
                && !domains
                    .iter()
                    .any(|saved| saved.eq_ignore_ascii_case(suffix))
            {
                domains.push(suffix);
            }
        }
        domains
    }

    /// 全部接口正在生效的原生 DNS（按接口顺序去重），容量投影由具体解析器负责。
    pub fn nameservers(&self) -> Vec<SocketAddr> {
        let mut servers = Vec::new();
        for server in self
            .interfaces
            .iter()
            .flat_map(|interface| &interface.dns)
            .map(|address| SocketAddr::new((*address).into(), 53))
            .chain(self.learned_dns.servers.iter().copied().map(SocketAddr::V6))
        {
            if !servers.contains(&server) {
                servers.push(server);
            }
        }
        servers
    }
}

/// DHCPv4 租约（dhcp-client → router_config 的事件）。
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
        self.u32(values.len() as u32);
        for value in values {
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
        self.u32(list.len() as u32);
        for suffix in list {
            self.bytes(suffix.as_bytes());
        }
        self
    }
    /// 写入一次配置提交及字段存在标记，返回当前编码器。
    pub fn update(&mut self, update: &Update<'_>) -> &mut Self {
        self.u32(VERSION);
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
        self.u32(VERSION);
        self.0.extend_from_slice(&snapshot.generation.to_bytes());
        self.u8(snapshot.configuration_status as u8)
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
            .learned_dns(&snapshot.learned_dns)
    }
    pub fn lease(&mut self, event: &LeaseEvent) -> &mut Self {
        self.u32(VERSION);
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
    /// 校验设置管理消息的当前线版本。
    /// # Errors
    /// 版本缺失或与当前布局不符。
    pub fn version(&mut self) -> Result<(), Invalid> {
        if self.u32()? == VERSION {
            Ok(())
        } else {
            Err(Invalid)
        }
    }
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
        let count = self.u32()? as usize;
        if count > self.0.len() / 4 {
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
    /// 解码当前原生接口投影；历史磁盘导入由配置 owner 的迁移器负责。
    ///
    /// # Errors
    ///
    /// 字段截断或值无效。
    pub fn config(&mut self) -> Result<InterfaceConfig, Invalid> {
        let method = match self.u8()? {
            0 => Method::Disabled,
            1 => Method::Dhcp,
            2 => Method::Static,
            _ => return Err(Invalid),
        };
        let config = InterfaceConfig {
            method,
            address: self.address()?,
            prefix_length: self.prefix()?,
            gateway: self.address()?,
            dns: self.addresses()?,
            dns_suffix: dns_suffix(self.bytes()?)?.into(),
        };
        config.validate()?;
        Ok(config)
    }
    /// 读取全局搜索列表，按提交顺序返回。
    ///
    /// # Errors
    ///
    /// 输入截断、域名无效或重复。
    pub fn search_list(&mut self) -> Result<Vec<String>, Invalid> {
        let count = self.u32()? as usize;
        if count > self.0.len() / 5 {
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
        self.version()?;
        let interface = self.bytes()?;
        let config = match self.u8()? {
            0 => None,
            1 => Some(self.config()?),
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
        let update = Update {
            interface,
            config,
            search_list,
            primary_dns_suffix,
            append_parent_suffixes,
        };
        update.validate()?;
        Ok(update)
    }
    pub fn snapshot(&mut self) -> Result<Snapshot, Invalid> {
        self.version()?;
        let generation = Revision::from_bytes(self.take(Revision::BYTES)?)?;
        let configuration_status = match self.u8()? {
            0 => ConfigurationStatus::Ready,
            1 => ConfigurationStatus::RollingBack,
            2 => ConfigurationStatus::Applying,
            3 => ConfigurationStatus::Rejected,
            _ => return Err(Invalid),
        };
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
                config: self.config()?,
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
            configuration_status,
            interfaces,
            search_list: self.search_list()?,
            primary_dns_suffix: dns_suffix(self.bytes()?)?.into(),
            append_parent_suffixes: match self.u8()? {
                0 => false,
                1 => true,
                _ => return Err(Invalid),
            },
            learned_dns: self.learned_dns()?,
        })
    }
    pub fn lease(&mut self) -> Result<LeaseEvent, Invalid> {
        self.version()?;
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
    /// 单次设置或转发连接等待的服务端时限。到期后客户端可按当前版本重新登记。
    pub const WATCH_SECONDS: u64 = 15;
    /// 连接观察等待名额暂满时的桥接状态，稍后可用相同游标重试。
    pub const CONNECTION_WATCH_BUSY: i32 = -3;
    /// 网关向原生服务转发修改及订阅的私有端点。
    pub const PATH: &[u8] = b"/dev/network-settings";
    /// 网关向原生服务读取状态的私有端点。
    pub const STATUS_PATH: &[u8] = b"/dev/network-status";
    /// 请求：`u32` 序号后接载荷；回复：`i32` 状态（0 成功，否则原生状态码）后接载荷。
    pub const HEADER_BYTES: usize = 4;
    /// 单个 datagram 的上限：请求与回复都必须装下。
    /// 包括帧头的设置投影消息容量；原生配置文档走独立文件能力，不受此值限制。
    pub const DATAGRAM_MAX: usize = 60 * 1024;
}

/// 配置管理 WebSocket 子协议。每次先传一个 bridge 控制消息，再按其长度传文档块。
pub mod management {
    /// 本地按接口抓包的控制帧与 PCAP 记录。
    pub mod capture;
    /// 单接口的流量构成及时间窗口统计。
    pub mod flows;
    pub(crate) mod page_delta;
    /// 链路流量观察的版本化页与变化帧。
    pub mod traffic;
    /// 本地 Unix 监听地址。
    pub const SOCKET_PATH: &str = "/run/router-api.sock";
    /// WebSocket 握手资源路径。
    pub const RESOURCE: &str = "/router";
    /// RFC 6455 的子协议名称；控制帧继续使用各自版本。
    pub const SUBPROTOCOL: &str = "liteos-router-v1";
    /// WebSocket 网关的防火墙计数订阅。该操作不转发给原生设置端点。
    pub const WATCH_FIREWALL_COUNTERS: u32 = 0x8000_0001;
    /// WebSocket 网关的邻居页订阅。该操作不转发给原生设置端点。
    pub const WATCH_NEIGHBORS: u32 = 0x8000_0002;
    /// WebSocket 网关的生效路由页订阅。
    pub const WATCH_ROUTES: u32 = 0x8000_0003;
    /// WebSocket 网关的接口流量观察。
    pub const WATCH_LINK_TRAFFIC: u32 = 0x8000_0004;
    /// 管理员从本地网关抓取指定接口的报文。
    pub const CAPTURE: u32 = 0x8000_0005;
    /// 管理员观察本地接口的聚合流量，网关不向观察者发送报文。
    pub const WATCH_FLOWS: u32 = 0x8000_0006;
    /// 采样观察会话的空闲保活标签。
    pub const OBSERVATION_HEARTBEAT: u8 = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn snapshot_round_trips_and_rejects_trailing_or_out_of_range_fields() {
        let mut config = InterfaceConfig::dhcp();
        config.dns = (1..=8).map(|last| Ipv4Addr::new(10, 0, 0, last)).collect();
        let snapshot = Snapshot {
            generation: Revision::initial(core::num::NonZeroU128::MIN),
            configuration_status: ConfigurationStatus::Applying,
            search_list: (0..8)
                .map(|index| alloc::format!("{}.zone{index}.test", "a".repeat(40)))
                .collect(),
            primary_dns_suffix: "sub.office.test".into(),
            append_parent_suffixes: true,
            learned_dns: dns::LearnedDns {
                servers: vec!["[fe80::2%7]:53".parse().unwrap()],
                domains: vec!["learned.test".into()],
            },
            interfaces: vec![InterfaceState {
                name: b"eth0".to_vec(),
                connection_name: "办公网".as_bytes().to_vec(),
                mac: [2, 0, 0, 0, 0, 1],
                config: config.clone(),
                address: Ipv4Addr::new(10, 0, 2, 15),
                prefix_length: 24,
                gateway: Ipv4Addr::new(10, 0, 2, 2),
                dns: config.dns,
                dns_suffix: "office.test".into(),
                connected_seconds: 42,
                lease: Some(LeaseStatus {
                    server: Ipv4Addr::new(10, 0, 2, 2),
                    obtained_at: 1_700_000_000,
                    expires_at: Some(1_700_000_600),
                }),
            }],
        };
        assert_eq!(snapshot.nameservers().len(), 9);
        assert_eq!(snapshot.search_domains().len(), 8);
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
        let mut bytes = encoder.0.clone();
        bytes[..4].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(Decoder::new(&bytes).snapshot(), Err(Invalid));
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4 + Revision::BYTES] = 4;
        assert_eq!(Decoder::new(&bytes).snapshot(), Err(Invalid));
        // 前缀长度位于两个名称、MAC、方法与地址之后。
        let prefix = 4 + Revision::BYTES + 1 + 4 + 4 + 4 + 4 + "办公网".len() + 6 + 1 + 4;
        let mut bytes = encoder.0;
        bytes[prefix] = 33;
        assert_eq!(Decoder::new(&bytes).snapshot(), Err(Invalid));
    }
}
