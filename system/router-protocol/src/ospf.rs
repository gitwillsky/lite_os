//! 持久 OSPF 配置、运行设备绑定及邻居观察模型。报文格式由 packet-wire 持有。

use crate::configuration::{self, DeviceMatch, InterfaceReference, Name, ValidationError};
pub use crate::secret::Secret;
use alloc::{collections::BTreeMap, format, vec::Vec};
use core::{
    net::{IpAddr, Ipv4Addr},
    num::NonZeroU16,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 原生配置工厂与状态接口的线版本。
pub const VERSION: u32 = 6;

/// 路由来源标识，对应 Linux 的 RTPROT_OSPF。
pub const ROUTE_PROTOCOL: u8 = 188;

/// 本地路由管理距离，和链路代价独立。
pub const PREFERENCE: u32 = 110;

/// 本路由实例的标识和按稳定接口名称配置的点到点链路。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    #[schemars(schema_with = "configuration::ipv4_schema")]
    pub router_id: Ipv4Addr,
    #[serde(deserialize_with = "configuration::unique_objects")]
    pub links: BTreeMap<Name, Link>,
}

/// 一条链路可独立启用 v2 与 v3，使用同一区域。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Link {
    #[schemars(schema_with = "configuration::ipv4_schema")]
    pub area: Ipv4Addr,
    #[serde(default = "default_cost")]
    pub cost: NonZeroU16,
    /// 仅通告接口网络，不在该链路发送 Hello 或建立邻接。
    #[serde(default)]
    pub passive: bool,
    pub v2: Option<Timers>,
    pub v3: Option<V3>,
    /// RFC 5709 HMAC-SHA-256 的密钥组。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v2_authentication: Option<Authentication>,
    /// RFC 7166 HMAC-SHA-256 的安全关联。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v3_authentication: Option<Authentication>,
}

/// 发送使用指定 ID，接收接受列表中的所有密钥，便于两端分步换钥。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Authentication {
    pub send_id: u16,
    #[serde(deserialize_with = "configuration::unique_objects")]
    pub keys: BTreeMap<u16, Secret>,
}

impl Authentication {
    fn valid(&self, maximum: u16) -> bool {
        self.keys.contains_key(&self.send_id) && self.keys.keys().all(|&id| id <= maximum)
    }
}

fn default_cost() -> NonZeroU16 {
    NonZeroU16::MIN
}

/// 邻居检测间隔，单位为秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Timers {
    pub hello: u16,
    pub dead: u32,
}

impl Timers {
    fn valid(self) -> bool {
        self.hello != 0 && self.dead > u32::from(self.hello)
    }
}

/// OSPFv3 IPv6 单播实例及其定时。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct V3 {
    #[serde(default)]
    pub instance: u8,
    pub timers: Timers,
}

impl Link {
    /// 判断链路版本、定时与认证配置是否可一起使用。接口引用由配置验证。
    pub fn valid(&self) -> bool {
        (self.v2.is_some() || self.v3.is_some())
            && self.v2.is_none_or(Timers::valid)
            && self.v3.is_none_or(|v3| {
                v3.instance < 32 && v3.timers.valid() && u16::try_from(v3.timers.dead).is_ok()
            })
            && self.v2_authentication.as_ref().is_none_or(|auth| {
                !self.passive && self.v2.is_some() && auth.valid(u16::from(u8::MAX))
            })
            && self
                .v3_authentication
                .as_ref()
                .is_none_or(|auth| !self.passive && self.v3.is_some() && auth.valid(u16::MAX))
    }
}

impl Configuration {
    /// 校验标识、定时器和可用于三层协议的接口配置引用。
    /// # Errors
    /// 非法 ID、无有效版本、间隔不合法、接口不存在或是物理网桥端口。
    pub fn validate(&self, router: &configuration::Configuration) -> Result<(), ValidationError> {
        self.validate_links(|name| routed_interface(router, name).is_some())
    }

    /// 校验标识、定时器及本域链路。`usable` 判断配置名能否用于三层协议。
    /// # Errors
    /// 路由器 ID、链路参数或接口引用无效。
    pub fn validate_links<'a>(
        &'a self,
        usable: impl Fn(&'a Name) -> bool,
    ) -> Result<(), ValidationError> {
        if self.router_id.is_unspecified() {
            return Err(ValidationError {
                path: "/ospf/router_id".into(),
                message: "OSPF router ID must be nonzero",
            });
        }
        if self.links.is_empty() {
            return Err(ValidationError {
                path: "/ospf/links".into(),
                message: "OSPF requires at least one link",
            });
        }
        for (name, link) in &self.links {
            let path = || {
                format!(
                    "/ospf/links/{}",
                    name.as_str().replace('~', "~0").replace('/', "~1")
                )
            };
            if !link.valid() {
                return Err(ValidationError {
                    path: path(),
                    message: "invalid OSPF version, instance, timers or authentication",
                });
            }
            if !usable(name) {
                return Err(ValidationError {
                    path: path(),
                    message: "expected a routed interface or VLAN gateway",
                });
            }
        }
        Ok(())
    }

    /// 将持久接口名解析为实际设备匹配条件，运行时索引由 NET 事实决定。
    /// # Errors
    /// 配置或接口引用无效。
    pub fn project(
        &self,
        router: &configuration::Configuration,
    ) -> Result<Session, ValidationError> {
        self.project_links([router.ipv4_forwarding, router.ipv6_forwarding], |name| {
            routed_interface(router, name)
        })
    }

    pub(crate) fn project_links<'a>(
        &'a self,
        forwarding: [bool; 2],
        resolve: impl Fn(&'a Name) -> Option<InterfaceReference<'a>>,
    ) -> Result<Session, ValidationError> {
        self.validate_links(|name| resolve(name).is_some())?;
        let mut links = BTreeMap::new();
        for (name, settings) in &self.links {
            let device = match resolve(name).expect("checked interface reference") {
                InterfaceReference::Device(device) => Binding::Physical(device.clone()),
                InterfaceReference::Vlan(name) => Binding::Vlan(name.clone()),
            };
            links.insert(
                name.clone(),
                BoundLink {
                    device,
                    settings: settings.clone(),
                },
            );
        }
        Ok(Session {
            router_id: self.router_id,
            forwarding,
            authentication_epoch: None,
            links,
        })
    }
}

fn routed_interface<'a>(
    router: &'a configuration::Configuration,
    name: &'a Name,
) -> Option<InterfaceReference<'a>> {
    if router
        .bridges
        .values()
        .any(|bridge| bridge.ports.contains_key(name))
    {
        return None;
    }
    router.interface_reference(name)
}

/// 设备的授权绑定条件，由配置 owner 一次投影。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    Physical(DeviceMatch),
    Vlan(Name),
}

/// 配置服务移交给协议实例的链路绑定和参数。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundLink {
    pub device: Binding,
    pub settings: Link,
}

/// 运行时实例的输入，按名称索引以保留可观察身份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub router_id: Ipv4Addr,
    /// IPv4、IPv6 的已提交转发状态。停用转发的地址族不生成本机通告。
    pub forwarding: [bool; 2],
    /// 已同步到稳定存储的 v3 发送序列高位，由配置服务租出。
    pub authentication_epoch: Option<u32>,
    #[serde(deserialize_with = "configuration::unique_objects")]
    pub links: BTreeMap<Name, BoundLink>,
}

impl Session {
    /// 是否需要为 RFC 7166 从持久存储保留发送序列空间。
    pub fn needs_authentication_epoch(&self) -> bool {
        self.links
            .values()
            .any(|link| link.settings.v3_authentication.is_some())
    }

    /// 将运行参数编码为原生工厂正文。
    /// # Errors
    /// JSON 序列化失败。
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// 解析工厂输入。
    /// # Errors
    /// 格式、参数或重复键非法。
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::Invalid> {
        let session: Self = serde_json::from_slice(bytes).map_err(|_| crate::Invalid)?;
        if session.router_id.is_unspecified()
            || session.links.is_empty()
            || session.links.values().any(|link| !link.settings.valid())
            || session.authentication_epoch.is_some_and(|epoch| epoch == 0)
            || session.authentication_epoch.is_some() != session.needs_authentication_epoch()
        {
            return Err(crate::Invalid);
        }
        Ok(session)
    }
}

/// 邻居发现阶段可报告的实际状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeighborState {
    Init,
    ExStart,
    Exchange,
    Loading,
    Full,
}

/// 一条已通过链路准入的邻居观察。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Neighbor {
    pub router_id: Ipv4Addr,
    pub address: IpAddr,
    pub state: NeighborState,
}

/// 单版本链路的本地地址及邻居，设备无可用地址时 `local` 缺席。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkStatus {
    pub name: Name,
    pub area: Ipv4Addr,
    pub version: u8,
    pub passive: bool,
    pub authentication: AuthenticationStatus,
    pub local: Option<IpAddr>,
    pub neighbors: Vec<Neighbor>,
}

/// 邻接诊断只包含认证模式和拒绝原因。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticationStatus {
    pub enabled: bool,
    pub rejected: u64,
    pub last_failure: Option<AuthenticationFailure>,
}

/// 最后一次认证未通过的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationFailure {
    Missing,
    Unexpected,
    Malformed,
    UnknownKey,
    InvalidDigest,
    Replay,
}

/// 授权查询的原生协议状态。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub links: Vec<LinkStatus>,
}

impl Snapshot {
    /// 编码当前观察正文。
    /// # Errors
    /// JSON 序列化失败。
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// 解析授权状态回复。
    /// # Errors
    /// 回复格式不符。
    pub fn parse(bytes: &[u8]) -> Result<Self, crate::Invalid> {
        serde_json::from_slice(bytes).map_err(|_| crate::Invalid)
    }
}
