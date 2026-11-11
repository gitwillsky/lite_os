//! 原生配置树的唯一数据模型。设备匹配与具名配置对象分离，不用 Linux IFNAMSIZ 作为配置身份。
//!
//! 策略由各执行领域消费；字段可编码或具有 Schema 不代表已经完成执行和系统验收。
//! JSON、强类型检查与 JSON Schema 共用这些声明；Linux 桌面设置仅为此模型的兼容投影。

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, net::Ipv4Addr};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, Visitor},
};

use crate::{InterfaceConfig, Invalid, Method};
mod delegation;
mod dhcp;
mod dns;
pub use dhcp::DhcpV4Pool;
pub use dns::{DnsForwarding, DnsListener, DnsUpstream};
mod ipv6;
mod policy_routing;
mod firewall;
mod rdnss;
pub use delegation::PrefixDelegation;
pub use ipv6::{
    ADVERTISEMENT_OPTION_SPACE, Ipv6Address, Ipv6Configuration, MAX_ADVERTISED_PREFIXES,
    RouterAdvertisementPolicy, RouterAdvertisements, valid_advertised_prefixes,
};
pub use firewall::{ConnectionState, Firewall, PreroutingAction, PreroutingRule};
pub use policy_routing::{PolicyRouteRule, PolicyRouting, RuleOrigin, StaticTable};
pub use rdnss::{AdvertisedDns, MAX_ADVERTISED_DNS, rdnss_size};
mod routes;
pub use routes::{StaticAction, StaticMember, StaticRoute, StaticTarget};

/// 持久配置树版本，与设置 RPC 和执行批次的线版本分别演进。
pub const VERSION: u32 = 1;
/// 原生以太网接口的默认 L3 MTU；具体设备仍在执行预备时校验自身能力。
pub const DEFAULT_MTU: u32 = 1500;
fn default_mtu() -> u32 {
    DEFAULT_MTU
}
/// DNS 文本形式上限（不含最后的根点）；不使用 musl 搜索行的较小容量。
const DOMAIN_MAX: usize = crate::DNS_SUFFIX_MAX;

/// 配置校验失败的位置与原因，路径按 JSON Pointer 表达，不把语法错误当成空配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ValidationError {
    /// JSON Pointer；空字符串表示根对象。
    pub path: String,
    /// 稳定的语义原因；展示层负责本地化。
    pub message: &'static str,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

fn invalid(path: impl Into<String>, message: &'static str) -> ValidationError {
    ValidationError {
        path: path.into(),
        message,
    }
}

/// 配置对象的稳定名字；不是执行接口名，可含非 ASCII 文本，不继承 Linux 的 15-byte 上限。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Name(String);

impl TryFrom<String> for Name {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(invalid("", "name is empty or contains control characters"));
        }
        Ok(Self(value))
    }
}
impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}
impl Name {
    /// 取得已检查的配置身份文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl JsonSchema for Name {
    fn schema_name() -> alloc::borrow::Cow<'static, str> {
        "ConfigurationName".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string", "minLength":1, "pattern":r"^[^\u0000-\u001F\u007F-\u009F]+$"})
    }
}

/// 精确设备匹配。MAC 绑定不会因为网卡枚举次序变化而指向另一块设备。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeviceMatch {
    /// 执行服务发布的设备名字，不是此对象的配置名。
    Name(Name),
    /// 六字节链路身份；字段无编码层私有设备 ID，跨启动不依赖临时 ifindex。
    Mac([u8; 6]),
}

impl DeviceMatch {
    /// 与执行服务当前发布的名字/MAC 匹配，不创建运行时绑定缓存。
    pub fn matches(&self, name: &[u8], mac: [u8; 6]) -> bool {
        match self {
            Self::Name(expected) => expected.as_str().as_bytes() == name,
            Self::Mac(expected) => *expected == mac,
        }
    }
}

/// IPv4 地址来源；管理开关属于接口，禁用接口不会丢掉原地址策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Assignment {
    #[default]
    Dhcp,
    Static,
}

/// IPv4 用户策略。未启用手动方法时保留原静态输入，以支持管理开关与方式切换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ipv4Configuration {
    /// 地址的取得方式。
    pub assignment: Assignment,
    /// 手动地址；自动取得时保留原输入但不应用。
    #[schemars(schema_with = "ipv4_schema")]
    pub address: Ipv4Addr,
    /// 手动地址的前缀长度。
    #[schemars(range(max = 32))]
    pub prefix_length: u8,
    /// 默认网关；全零表示不添加默认路由。
    #[schemars(schema_with = "ipv4_schema")]
    pub gateway: Ipv4Addr,
    /// 完整原生列表；具体 libc 解析器的较小容量不属于此配置类型。
    #[schemars(schema_with = "ipv4_list_schema")]
    pub dns: Vec<Ipv4Addr>,
    /// 空值使用租约提供的连接后缀。
    #[schemars(length(max = DOMAIN_MAX))]
    pub dns_suffix: String,
}

// Schemars 的内置 IP 描述受 std feature 限制；Serde 的 core::net 编码仍为标准文本。
// 只在元数据中描述这一相同格式，不添加另一种地址表示或解析器。
pub(crate) fn address_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"string","format":"ipv4"},{"type":"string","format":"ipv6"}]})
}
pub(crate) fn optional_address_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"string","format":"ipv4"},{"type":"string","format":"ipv6"},{"type":"null"}]})
}
pub(crate) fn ipv4_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string", "format":"ipv4"})
}
pub(crate) fn optional_ipv4_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"string","format":"ipv4"},{"type":"null"}]})
}
fn ipv4_list_schema(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"array", "items":ipv4_schema(generator)})
}

impl Ipv4Configuration {
    /// 从兼容接口设置转入原生策略；是否管理性启用由外层接口单独持有。
    pub fn from_projection(config: &InterfaceConfig) -> Self {
        Self {
            assignment: if config.method == Method::Static
                || config.method == Method::Disabled && !config.address.is_unspecified()
            {
                Assignment::Static
            } else {
                Assignment::Dhcp
            },
            address: config.address,
            prefix_length: config.prefix_length,
            gateway: config.gateway,
            dns: config.dns.clone(),
            dns_suffix: config.dns_suffix.clone(),
        }
    }

    fn project(&self, enabled: bool) -> InterfaceConfig {
        InterfaceConfig {
            method: if !enabled {
                Method::Disabled
            } else if self.assignment == Assignment::Static {
                Method::Static
            } else {
                Method::Dhcp
            },
            address: self.address,
            prefix_length: self.prefix_length,
            gateway: self.gateway,
            dns: self.dns.clone(),
            dns_suffix: self.dns_suffix.clone(),
        }
    }
}

/// 一个具名接口配置对象。可只覆盖显示名，其余继续继承默认策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    /// 默认路由的偏好/代价覆盖；None 使用配置服务默认策略，不在文件中保存运行时派生值。
    #[serde(default)]
    pub default_route: Option<crate::routing::Attributes>,
    /// L3 MTU；省略时使用以太网默认值，删除覆盖后同样恢复默认，不沿用旧运行状态。
    #[serde(default = "default_mtu")]
    #[schemars(
        range(min = 68, max = 65535),
        title = "最大数据包大小（MTU）",
        description = "单位为字节；具体可用范围由网卡能力决定。"
    )]
    pub mtu: u32,
    /// 选定实际设备的匹配条件。
    pub device: DeviceMatch,
    /// `None` 继承默认管理状态。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `None` 使用桌面默认显示名。
    #[serde(default)]
    pub display_name: Option<String>,
    /// `None` 继承 bootfs 默认策略，而非保存一份默认值副本。
    #[serde(default)]
    pub ipv4: Option<Ipv4Configuration>,
    /// 仅在此接口启用时分发 DHCPv4 地址。
    #[serde(default)]
    pub dhcp_v4_server: Option<DhcpV4Pool>,
    /// 静态 IPv6 地址集合；缺省没有手动地址，自动链路地址仍由执行 owner 管理。
    #[serde(default)]
    pub ipv6: Option<Ipv6Configuration>,
}

impl Interface {
    /// 将原生期望策略投影给现有 IPv4 执行/桌面接口，不改写完整原生配置。
    pub fn project(&self, inherited: InterfaceConfig) -> InterfaceConfig {
        let enabled = self.enabled.unwrap_or(inherited.method != Method::Disabled);
        if let Some(ipv4) = &self.ipv4 {
            ipv4.project(enabled)
        } else {
            let mut inherited = inherited;
            if !enabled {
                inherited.method = Method::Disabled;
            } else if inherited.method == Method::Disabled {
                inherited.method = if inherited.address.is_unspecified() {
                    Method::Dhcp
                } else {
                    Method::Static
                };
            }
            inherited
        }
    }
}

/// 系统解析策略，独立于某一 Linux 解析器的行缓冲上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsConfiguration {
    /// 下游 DNS 转发与缓存；缺席时不监听。
    #[serde(default)]
    pub forwarding: Option<DnsForwarding>,
    /// 有序搜索域；空列表使用自动搜索策略。
    pub search_list: Vec<String>,
    /// 主 DNS 后缀；空值表示没有。
    #[schemars(length(max = DOMAIN_MAX))]
    pub primary_suffix: String,
    /// 是否在自动搜索时依次尝试主后缀的父域。
    pub append_parent_suffixes: bool,
}
impl Default for DnsConfiguration {
    fn default() -> Self {
        Self {
            forwarding: None,
            search_list: Vec::new(),
            primary_suffix: String::new(),
            append_parent_suffixes: true,
        }
    }
}

/// 原生配置树；唯一持久副本，不存 DHCP 租约、运行时 ifindex 或转发状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// 配置格式版本，必须为当前版本。
    #[schemars(range(min = VERSION, max = VERSION))]
    pub version: u32,
    /// 按稳定配置名索引的接口策略。
    #[serde(default, deserialize_with = "unique_objects")]
    pub interfaces: BTreeMap<Name, Interface>,
    /// 是否转发目的不是本机的 IPv4 单播；路由器缺省开启，不影响本机 socket 或路由学习。
    #[serde(default = "forwarding_default")]
    #[schemars(
        title = "IPv4 转发",
        description = "允许在接口之间转发 IPv4 单播；关闭仍保留路由和本机通信。"
    )]
    pub ipv4_forwarding: bool,
    /// 是否转发 IPv6 单播；独立于 IPv4 开关，关闭仍保留本机通信和路由学习。
    #[serde(default = "forwarding_default")]
    #[schemars(
        title = "IPv6 转发",
        description = "允许在接口之间转发 IPv6 单播；不转发链路范围地址。"
    )]
    pub ipv6_forwarding: bool,
    /// 上联 IPv6 前缀委派及下游子网编号。缺席时停用。
    #[serde(default)]
    pub prefix_delegation: Option<PrefixDelegation>,
    /// 双栈静态来源；接口使用配置对象名称，绝不持久化运行时 ifindex。
    #[serde(default)]
    pub routes: Vec<StaticRoute>,
    /// 按流量来源、源网段和入接口选择双栈路由表。
    #[serde(default)]
    pub policy_routing: PolicyRouting,
    /// 双栈防火墙规则；缺席使用空链。
    #[serde(default, skip_serializing_if = "Firewall::is_empty")]
    pub firewall: Firewall,
    /// 受管来源的有序准入策略；缺省保持全部路由，实际接口观测事实不受此策略修改。
    #[serde(default)]
    pub route_policy: crate::policy::Policy,
    /// BGP 会话域；None 停止全部邻居。路由进出需要对应服务实际支持并显式配置的策略。
    #[serde(default)]
    pub bgp: Option<crate::bgp::Configuration>,
    /// 系统解析策略。
    #[serde(default)]
    pub dns: DnsConfiguration,
    /// 自动备份周期与保留数量；None 禁用，已有备份不会因禁用而删除。
    #[serde(default)]
    #[schemars(
        title = "自动备份",
        description = "设置备份周期与保留份数；关闭时保留已有备份。"
    )]
    pub backup: Option<crate::backup::Policy>,
}

/// 返回路由器的缺省单播转发策略；配置缺省与原生域初始值共用此定义，不分配。
pub const fn forwarding_default() -> bool {
    true
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            version: VERSION,
            ipv4_forwarding: forwarding_default(),
            ipv6_forwarding: forwarding_default(),
            prefix_delegation: None,
            route_policy: crate::policy::Policy::default(),
            interfaces: BTreeMap::new(),
            routes: Vec::new(),
            policy_routing: PolicyRouting::default(),
            firewall: Firewall::default(),
            bgp: None,
            dns: DnsConfiguration::default(),
            backup: None,
        }
    }
}

/// 捕获按配置名索引的对象；重复 JSON 键必须拒绝，不能静默覆盖另一个接口或邻居。
pub(crate) fn unique_objects<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    decoder: D,
) -> Result<BTreeMap<Name, T>, D::Error> {
    struct Objects<T>(core::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Objects<T> {
        type Value = BTreeMap<Name, T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("uniquely named configuration objects")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut objects = BTreeMap::new();
            while let Some((name, interface)) = map.next_entry()? {
                if objects.insert(name, interface).is_some() {
                    return Err(de::Error::custom("duplicate configuration object name"));
                }
            }
            Ok(objects)
        }
    }
    decoder.deserialize_map(Objects(core::marker::PhantomData))
}

fn domain(value: &str) -> bool {
    crate::dns_suffix(value.as_bytes()).is_ok()
}

impl Configuration {
    /// 完整语义校验，与 JSON 导入及任何服务内构造共用。
    /// # Errors
    /// 未知版本、重复设备/显示名、不合法地址策略、域名或重复搜索域。
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.version != VERSION {
            return Err(invalid("/version", "unsupported configuration version"));
        }
        if let Some(forwarding) = &self.dns.forwarding {
            forwarding.validate(self)?;
        }
        if let Some(delegation) = &self.prefix_delegation {
            delegation.validate(self)?;
        }
        if let Some(bgp) = &self.bgp {
            bgp.validate()?;
        }
        self.route_policy.validate().map_err(|error| {
            invalid(
                format!("/route_policy/rules/{}", error.index),
                error.message,
            )
        })?;
        let mut devices = BTreeSet::new();
        routes::validate(self, &self.routes, "/routes")?;
        self.policy_routing.validate(self)?;
        self.validate_firewall()?;
        let mut labels = BTreeSet::new();
        for (name, interface) in &self.interfaces {
            let path = format!(
                "/interfaces/{}",
                name.as_str().replace('~', "~0").replace('/', "~1")
            );
            if let Some(ipv6) = &interface.ipv6 {
                ipv6.validate().map_err(|mut error| {
                    error.path = format!("{path}{}", error.path);
                    error
                })?;
            }
            if interface.dhcp_settings().is_err() {
                return Err(invalid(
                    format!("{path}/dhcp_v4_server"),
                    "requires static IPv4, IPv4 MTU of at least 576 and a valid on-link pool",
                ));
            }
            if !devices.insert(&interface.device) {
                return Err(invalid(
                    format!("{path}/device"),
                    "device matched by multiple objects",
                ));
            }
            if let Some(label) = &interface.display_name
                && (crate::connection_name(label.as_bytes()).is_err()
                    || !labels.insert(label.to_lowercase()))
            {
                return Err(invalid(
                    format!("{path}/display_name"),
                    "invalid or duplicate display name",
                ));
            }
            if !(68..=65535).contains(&interface.mtu) {
                return Err(invalid(format!("{path}/mtu"), "invalid Ethernet MTU"));
            }
            if interface
                .default_route
                .is_some_and(|attributes| attributes.preference == 0)
            {
                return Err(invalid(
                    format!("{path}/default_route/preference"),
                    "preference must be nonzero",
                ));
            }
            if let Some(ipv4) = &interface.ipv4 {
                if ipv4.prefix_length > 32
                    || interface.enabled != Some(false)
                        && ipv4.assignment == Assignment::Static
                        && ipv4.address.is_unspecified()
                {
                    return Err(invalid(
                        format!("{path}/ipv4"),
                        "invalid IPv4 address or prefix",
                    ));
                }
                if !domain(&ipv4.dns_suffix) {
                    return Err(invalid(
                        format!("{path}/ipv4/dns_suffix"),
                        "invalid DNS suffix",
                    ));
                }
            }
        }
        if !domain(&self.dns.primary_suffix) {
            return Err(invalid("/dns/primary_suffix", "invalid DNS suffix"));
        }
        let mut suffixes = BTreeSet::new();
        for (index, suffix) in self.dns.search_list.iter().enumerate() {
            if suffix.is_empty() || !domain(suffix) || !suffixes.insert(suffix.to_ascii_lowercase())
            {
                return Err(invalid(
                    format!("/dns/search_list/{index}"),
                    "invalid or duplicate search domain",
                ));
            }
        }
        Ok(())
    }

    /// 查找唯一匹配当前硬件事实的对象；名字与 MAC 两种匹配若发生别名冲突必须报错。
    /// # Errors
    /// 多个配置对象匹配同一实际接口。
    pub fn matching(&self, name: &[u8], mac: [u8; 6]) -> Result<Option<&Interface>, Invalid> {
        let mut matching = self
            .interfaces
            .values()
            .filter(|interface| interface.device.matches(name, mac));
        let result = matching.next();
        if matching.next().is_some() {
            Err(Invalid)
        } else {
            Ok(result)
        }
    }

    /// 取得匹配对象的可变引用，或为明确选定的当前设备创建具名对象。
    /// # Errors
    /// 名字无效、匹配含糊或自动对象名与已有其他对象冲突。
    pub fn ensure_interface(
        &mut self,
        name: &[u8],
        mac: Option<[u8; 6]>,
    ) -> Result<&mut Interface, Invalid> {
        let mut matching = self.interfaces.iter().filter(|(_, interface)| match mac {
            Some(mac) => interface.device.matches(name, mac),
            None => matches!(&interface.device, DeviceMatch::Name(expected) if expected.as_str().as_bytes() == name),
        });
        let found = matching.next().map(|(name, _)| name.clone());
        if matching.next().is_some() {
            return Err(Invalid);
        }
        let key = if let Some(found) = found {
            found
        } else {
            let name = core::str::from_utf8(name).map_err(|_| Invalid)?.to_string();
            let mut key = Name::try_from(name.clone()).map_err(|_| Invalid)?;
            if self.interfaces.contains_key(&key) {
                let mac = mac.ok_or(Invalid)?;
                key = Name::try_from(format!(
                    "{name}@{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                ))
                .map_err(|_| Invalid)?;
                if self.interfaces.contains_key(&key) {
                    return Err(Invalid);
                }
            }
            self.interfaces.insert(
                key.clone(),
                Interface {
                    default_route: None,
                    mtu: DEFAULT_MTU,
                    device: match mac {
                        Some(mac) => DeviceMatch::Mac(mac),
                        None => DeviceMatch::Name(Name::try_from(name).map_err(|_| Invalid)?),
                    },
                    enabled: None,
                    display_name: None,
                    ipv4: None,
                    dhcp_v4_server: None,
                    ipv6: None,
                },
            );
            key
        };
        let interface = self
            .interfaces
            .get_mut(&key)
            .expect("configuration object selected or inserted");
        // 明确修改当前设备时把历史名字绑定落实为已知硬件身份；仅改显示名不冻结默认 IP 策略。
        if let Some(mac) = mac {
            interface.device = DeviceMatch::Mac(mac);
        }
        Ok(interface)
    }

    /// 把已验证配置投影为完整静态来源；设备身份只通过本次解析获得，缺席成员暂退而不删除持久意图。
    /// # Parameters
    /// `family` 只投影该地址族，空集合明确撤回；`resolve` 依据稳定设备匹配返回当前 ifindex，含糊时返回 Invalid。
    /// # Returns
    /// 唯一 routing::Batch 格式的独占正文；无静态路由时为显式空来源。
    /// # Errors
    /// 名称/设备解析、正文/数量非法或内存不足。
    pub fn route_source(
        &self,
        family: crate::routing::Family,
        resolve: impl FnMut(&DeviceMatch) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, crate::routing::EncodeError> {
        routes::compile(self, &self.routes, family, resolve)
    }

    /// 从 JSON 导入并执行完整语义检查，拒绝未知字段、重复对象键及尾部内容。
    /// # Errors
    /// JSON 语法/字段或配置语义错误。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let configuration: Self =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        configuration
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(configuration)
    }
    /// 编码唯一当前 JSON 格式；服务内构造也必须通过同一语义校验。
    /// # Errors
    /// 语义无效或编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate().map_err(|error| error.to_string())?;
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }
    /// 从相同类型声明生成固定 Draft 2020-12 元数据，不手工复制字段清单或全局缓存。
    pub fn schema() -> Schema {
        schemars::generate::SchemaSettings::draft2020_12()
            .into_generator()
            .into_root_schema_for::<Self>()
    }

    /// 编码与当前类型一致的字段元数据，不持有全局缓存。
    /// # Errors
    /// JSON 编码失败。
    pub fn schema_json() -> Result<Vec<u8>, String> {
        serde_json::to_vec(&Self::schema()).map_err(|error| error.to_string())
    }

    /// 对比两个有效配置，返回有序 JSON 变更列表；对象逐字段比较，有序数组整体替换。
    ///
    /// 每项含 RFC 6901 `path` 及存在时的 `before`/`after`，省略一侧表示添加/删除，JSON null 仍为真实值。
    /// 只提供审阅结果，不把对比输出当成可绕过配置语义校验的补丁执行入口。
    /// # Errors
    /// 任一配置无效或 JSON 编码失败。
    pub fn difference_json(&self, next: &Self) -> Result<Vec<u8>, String> {
        self.validate().map_err(|error| error.to_string())?;
        next.validate().map_err(|error| error.to_string())?;
        let before = serde_json::to_value(self).map_err(|error| error.to_string())?;
        let after = serde_json::to_value(next).map_err(|error| error.to_string())?;
        let mut changes = Vec::new();
        difference("", Some(&before), Some(&after), &mut changes);
        serde_json::to_vec(&changes).map_err(|error| error.to_string())
    }
}

#[derive(Serialize)]
struct Change<'a> {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<&'a serde_json::Value>,
}

fn difference<'a>(
    path: &str,
    before: Option<&'a serde_json::Value>,
    after: Option<&'a serde_json::Value>,
    changes: &mut Vec<Change<'a>>,
) {
    if let (Some(serde_json::Value::Object(before)), Some(serde_json::Value::Object(after))) =
        (before, after)
    {
        for key in before
            .keys()
            .chain(after.keys().filter(|key| !before.contains_key(*key)))
        {
            let path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
            difference(&path, before.get(key), after.get(key), changes);
        }
    } else if before != after {
        changes.push(Change {
            path: path.into(),
            before,
            after,
        });
    }
}

#[cfg(test)]
mod tests;
