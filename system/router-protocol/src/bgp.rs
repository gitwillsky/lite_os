//! BGP 会话配置及观察值；只定义数据，运行时连接/能力不进入持久配置。
mod authentication;
mod confederation;
pub use authentication::Md5Password;
mod expression;
use crate::{
    Invalid,
    configuration::{Name, ValidationError},
};
use alloc::{boxed::Box, collections::BTreeMap, format, string::String, vec::Vec};
pub use confederation::{Confederation, PeerKind};
use core::{
    net::{IpAddr, Ipv4Addr},
    num::{NonZeroU16, NonZeroU32},
    ops::RangeInclusive,
};
pub use expression::AsPathExpression;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod communities;
mod routes;
pub use communities::CommunityEdit;
pub use routes::{
    ExportObservation, PathAttributes, PathSegment, RouteDecision, RouteObservation, Routes,
    Selected,
};

/// 原生 BGP 配置/状态传输版本；与 BGP 网络报文版本无关。
pub const VERSION: u32 = 19;
/// BGP 领域提供的路由观测标记；实际安装权限仍由来源能力独立授予。
pub const ROUTE_PROTOCOL: u8 = 186;

/// 向指定邻居请求重新发布一个已协商的单播地址族，不修改持久配置。
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    pub neighbor: Name,
    pub family: crate::routing::Family,
}
impl RefreshRequest {
    /// 编码供配置服务和会话实例共用的请求文档。
    /// # Errors
    /// 文档编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        serde_json::to_vec(self).map_err(|_| Invalid)
    }
    /// 验证请求文档的完整内容和邻居配置名。
    /// # Errors
    /// 未知地址族、空名称、非法 JSON 或未知字段。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        serde_json::from_slice(bytes).map_err(|_| Invalid)
    }
}

/// 单个路由域的会话参数；TCP 地址族与协商的单播路由族相互独立。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    /// 本路由域的实际 AS；配置联盟时是本成员 AS，外部身份由 confederation 提供。
    pub local_as: NonZeroU32,
    /// 联盟身份与其他成员；缺省为普通自治系统。
    #[serde(default)]
    pub confederation: Option<Box<Confederation>>,
    /// 非零 BGP Identifier，按整数身份使用，不要求属于本机接口。
    #[schemars(schema_with = "crate::configuration::ipv4_schema")]
    pub identifier: Ipv4Addr,
    /// 显式反射簇身份；未指定但有客户端时使用本地 Identifier，不把它当作接口地址。
    #[serde(default)]
    #[schemars(schema_with = "crate::configuration::optional_ipv4_schema")]
    pub cluster_id: Option<Ipv4Addr>,
    /// 仅在统一 RIB 存在精确可用路由时宣告的本机双栈网段；出口政策仍须显式允许。
    #[serde(default, deserialize_with = "networks")]
    pub networks: Vec<crate::tracking::IpPrefix>,
    /// 显式选择连接前缀及其他协议来源；缺省不重分发，BGP 自身来源不允许导回。
    #[serde(default)]
    pub redistribute: Option<Box<crate::tracking::routes::Selector>>,
    /// 稳定配置名，重复 JSON 身份拒绝。
    #[serde(deserialize_with = "crate::configuration::unique_objects")]
    pub neighbors: BTreeMap<Name, Neighbor>,
}

fn default_ipv4_unicast() -> bool {
    true
}

/// 邻居的期望配置；名称与连接的临时身份分开，不持久化 PID、句柄或 ifindex。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Neighbor {
    /// 实际 TCP 邻居；使用原生 IPv4/IPv6 地址，不持久化运行时 zone 编号。
    #[schemars(schema_with = "crate::configuration::address_schema")]
    pub address: IpAddr,
    /// 链路本地 IPv6 邻居的实际接口名；其他地址不设置范围，每次连接按当前接口身份解析。
    #[serde(default)]
    pub interface: Option<Name>,
    /// TCP MD5 认证；未设置使用普通 TCP，设置后所有握手/数据/关闭报文都必须验签。
    #[serde(default)]
    pub md5_password: Option<Box<Md5Password>>,
    /// 期望的实际 AS，须与 OPEN 一致。
    pub asn: NonZeroU32,
    /// 原生管理距离；只有显式准入的路由才使用，不是 LOCAL_PREF。
    pub preference: NonZeroU32,
    /// 期望 hold time，0 禁用；1/2 无效。
    pub hold_seconds: u16,
    /// 非零连接重试周期。
    pub retry_seconds: NonZeroU16,
    /// 只接入站 TCP，不主动发起。
    pub passive: bool,
    /// 手工指定此 iBGP 邻居为反射器客户端；eBGP 不允许该角色，缺省保持非客户端。
    #[serde(default)]
    pub route_reflector_client: bool,
    /// 在此 TCP 连接协商 IPv4 单播路由；不要求 TCP 使用 IPv4，缺省启用。
    #[serde(default = "default_ipv4_unicast")]
    pub ipv4_unicast: bool,
    /// 在此 TCP 连接协商 IPv6 单播路由，独立于 TCP 传输地址族。
    #[serde(default)]
    pub ipv6_unicast: bool,
    /// 显式入口策略；缺省不采用此邻居路由，接收表仍保留原始属性。
    #[serde(default)]
    pub import: Option<ImportPolicy>,
    /// 是否允许通过默认路由解析 BGP 下一跳；缺省拒绝，不暗借默认出口证明可达。
    #[serde(default)]
    pub resolve_via_default: bool,
    /// 显式出口策略；缺省拒绝，不因已经允许入口而自动向其他邻居传播。
    #[serde(default)]
    pub export: Option<ExportPolicy>,
    /// 同前缀宣告/撤回的最短间隔（秒）；缺省 eBGP 30/iBGP 5，零显式关闭。
    #[serde(default)]
    pub advertisement_seconds: Option<u16>,
}

/// 有序 first-match 入口规则；默认决定与本地 degree 均由配置 owner 明确提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportPolicy {
    /// 没有规则命中时的明确决定。
    pub default: crate::policy::Disposition,
    /// 外部路由未被规则覆盖时的本地 degree；内部路由优先保留其 LOCAL_PREF。
    pub preference: u32,
    /// 有序规则；多个匹配条件取 AND。
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}
/// 有序出口规则；准入与必要的后政策覆盖复用同一规则模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportPolicy {
    /// 没有规则命中时的明确决定。
    pub default: crate::policy::Disposition,
    /// 首个匹配终结；协议去环、分割水平与保留团体限制不能由 permit 绕过。
    #[serde(default)]
    pub rules: Vec<RouteRule>,
}
/// 一条路由属性规则，团体及 AS 条件均表示包含全部指定值，不创建另一套路径表示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    /// 匹配改写前的 ORIGIN；未设置为通配。
    #[serde(default)]
    pub origin: Option<Origin>,
    /// 包含两端的 MED 范围（start/end）；缺失 MED 不匹配，不把选路时的缺省零伪装成属性。
    #[serde(default)]
    pub med: Option<RangeInclusive<u32>>,
    /// 匹配进入本策略时的实际本地优先值；入口采用已计算的初始 degree，出口采用 Loc-RIB 确认值。
    #[serde(default)]
    pub local_preference: Option<RangeInclusive<u32>>,
    /// 包含两端的 AS 跳数范围；AS_SEQUENCE 按成员计数，AS_SET 为一跳，联盟段为零。
    #[serde(default)]
    pub as_path_length: Option<RangeInclusive<u32>>,
    /// 可选目的及长度范围，共用原生策略前缀模型。
    #[serde(default)]
    pub prefix: Option<crate::policy::Prefix>,
    /// 有效四字节 AS_PATH 必须包含的 AS；0 无效。
    #[serde(default)]
    pub as_path_contains: Vec<NonZeroU32>,
    /// 对有效路径的 AS-plain 文本作正则筛选；下划线表示 AS 边界，与其他条件取 AND。
    /// 捕获时完整编译为不可变程序，非法或超出编译资源预算的表达式不能进入配置。
    #[serde(default)]
    pub as_path_regex: Option<AsPathExpression>,
    /// 普通、扩展及大团体的集合条件。
    #[serde(default)]
    pub communities: Vec<u32>,
    #[serde(default)]
    pub extended_communities: Vec<[u8; 8]>,
    #[serde(default)]
    pub large_communities: Vec<[u8; 12]>,
    /// 首个命中规则的终结动作。
    pub action: RouteAction,
}
/// ORIGIN 的标准取值；用于真实路径选择和传播，不接纳未知编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Origin {
    Igp = 0,
    Egp = 1,
    Incomplete = 2,
}
/// MED 的显式设置或删除；未配置保持原值，删除不等于设置零。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MedAction {
    Set { value: u32 },
    Remove,
}
/// 替换路径中的全部匹配 AS；from 缺省匹配全部，to 必须为显式非零 ASN。
/// 序列保留顺序和重复次数，集合按替换后的成员关系规范化；不隐式使用本地 AS。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AsPathReplacement {
    /// 只替换此 ASN；None 明确表示替换全部幸存 ASN。
    #[serde(default)]
    pub from: Option<NonZeroU32>,
    /// 显式目标 ASN，不从邻居或本地配置隐式推导。
    pub to: NonZeroU32,
}
/// 路由政策不改写原始属性；degree 与逻辑下一跳属于后政策投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteAction {
    Deny,
    Permit {
        /// 覆盖本地 degree；Some(0) 也是有效值，不是管理距离。
        #[serde(default)]
        preference: Option<u32>,
        /// 覆盖同族全局下一跳；链路地址缺少 zone，不能由策略猜接口或伪造可达。
        #[serde(default)]
        #[schemars(schema_with = "crate::configuration::optional_address_schema")]
        next_hop: Option<IpAddr>,
        /// 改写后政策 ORIGIN，原始接收属性仍保留。
        #[serde(default)]
        origin: Option<Origin>,
        /// 显式修改 MED；出口设置表示本地重发，不传播旧的跨 AS MED。
        #[serde(default)]
        med: Option<MedAction>,
        /// 在首个普通路径段前 prepend 有序 AS 列表；保留重复，0 无效，空列表不改变路径。
        #[serde(default)]
        as_path_prepend: Option<Box<[NonZeroU32]>>,
        /// 在前置操作之前删除所有段中指定 AS 的全部出现；解码时排序去重，空列表保持。
        #[serde(default, deserialize_with = "path_exclusions")]
        as_path_exclude: Option<Box<[NonZeroU32]>>,
        /// 删除之后、前置之前替换指定/全部 AS；只为配置此动作的规则分配对象。
        #[serde(default)]
        as_path_replace: Option<Box<AsPathReplacement>>,
        /// 团体集合编辑；缺省保持，空 replace 清空，不修改原始接收。
        /// 只为已配置编辑分配 Box，避免每条普通/拒绝规则都内嵌三个 72-byte 向量头。
        #[serde(default)]
        communities: Option<Box<CommunityEdit<u32>>>,
        #[serde(default)]
        extended_communities: Option<Box<CommunityEdit<[u8; 8]>>>,
        #[serde(default)]
        large_communities: Option<Box<CommunityEdit<[u8; 12]>>>,
    },
}
fn networks<'de, D>(deserializer: D) -> Result<Vec<crate::tracking::IpPrefix>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut values = Vec::<crate::tracking::IpPrefix>::deserialize(deserializer)?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}
// 配置只规范化一次；逐路由用二分成员查询，不改变路径本身的顺序和重复次数。
fn path_exclusions<'de, D>(deserializer: D) -> Result<Option<Box<[NonZeroU32]>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut values = Option::<Vec<NonZeroU32>>::deserialize(deserializer)?;
    if let Some(values) = &mut values {
        values.sort_unstable();
        values.dedup();
    }
    Ok(values.map(Vec::into_boxed_slice))
}
impl Configuration {
    /// 校验唯一邻居地址、Identifier、计时及现有会话核心的参数表示范围；不建立连接。
    /// # Errors
    /// 地址、Identifier、hold/retry 或配置身份非法。
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(confederation) = &self.confederation {
            confederation.validate()?;
        }
        if self.identifier.is_unspecified() {
            return Err(ValidationError {
                path: "/bgp/identifier".into(),
                message: "identifier must be nonzero",
            });
        }
        if self.networks.iter().any(|network| !network.valid())
            || self.networks.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(ValidationError {
                path: "/bgp/networks".into(),
                message: "networks must be canonical ordered IP prefixes",
            });
        }
        if self.redistribute.as_ref().is_some_and(|selector| {
            !selector.valid() || selector.protocols.binary_search(&ROUTE_PROTOCOL).is_ok()
        }) {
            return Err(ValidationError {
                path: "/bgp/redistribute".into(),
                message: "redistribution requires explicit sources and must exclude BGP",
            });
        }
        let reflector = self.reflection_cluster().is_some();
        let mut addresses = alloc::collections::BTreeSet::new();
        for (name, neighbor) in &self.neighbors {
            let path = format!(
                "/bgp/neighbors/{}",
                name.as_str().replace('~', "~0").replace('/', "~1")
            );
            if neighbor.address.is_unspecified()
                || neighbor.address.is_multicast()
                || matches!(neighbor.address,IpAddr::V4(ip) if ip.is_broadcast())
                || matches!(neighbor.address,IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some())
            {
                return Err(ValidationError {
                    path: format!("{path}/address"),
                    message: "invalid neighbor transport address",
                });
            }
            let scoped = matches!(neighbor.address,IpAddr::V6(ip) if ip.is_unicast_link_local());
            if scoped != neighbor.interface.is_some() {
                return Err(ValidationError {
                    path: format!("{path}/interface"),
                    message: "link-local IPv6 peers require an interface; other peers have no interface scope",
                });
            }
            if !neighbor.ipv4_unicast && !neighbor.ipv6_unicast {
                return Err(ValidationError {
                    path: format!("{path}/ipv4_unicast"),
                    message: "at least one unicast address family must be enabled",
                });
            }
            if !addresses.insert((neighbor.address, neighbor.interface.as_ref())) {
                return Err(ValidationError {
                    path: format!("{path}/address"),
                    message: "duplicate neighbor transport address",
                });
            }
            if neighbor.route_reflector_client && neighbor.asn != self.local_as {
                return Err(ValidationError {
                    path: format!("{path}/route_reflector_client"),
                    message: "route reflector client must be an internal neighbor",
                });
            }
            if matches!(neighbor.hold_seconds, 1 | 2) {
                return Err(ValidationError {
                    path: format!("{path}/hold_seconds"),
                    message: "hold time must be zero or at least three seconds",
                });
            }
            for (direction, rules) in neighbor
                .import
                .iter()
                .map(|policy| ("import", &policy.rules))
                .chain(
                    neighbor
                        .export
                        .iter()
                        .map(|policy| ("export", &policy.rules)),
                )
            {
                for (index, rule) in rules.iter().enumerate() {
                    for (field, range) in [
                        ("med", rule.med.as_ref()),
                        ("local_preference", rule.local_preference.as_ref()),
                        ("as_path_length", rule.as_path_length.as_ref()),
                    ] {
                        if range.is_some_and(RangeInclusive::is_empty) {
                            return Err(ValidationError {
                                path: format!("{path}/{direction}/rules/{index}/{field}"),
                                message: "attribute range must include at least one value",
                            });
                        }
                    }
                    if rule
                        .prefix
                        .as_ref()
                        .is_some_and(|prefix| prefix.lengths().is_none())
                    {
                        return Err(ValidationError {
                            path: format!("{path}/{direction}/rules/{index}/prefix"),
                            message: "invalid policy prefix or length range",
                        });
                    }
                    if let RouteAction::Permit {
                        communities,
                        extended_communities,
                        large_communities,
                        ..
                    } = &rule.action
                        && (communities
                            .as_ref()
                            .is_some_and(|edit| !edit.is_canonical())
                            || extended_communities
                                .as_ref()
                                .is_some_and(|edit| !edit.is_canonical())
                            || large_communities
                                .as_ref()
                                .is_some_and(|edit| !edit.is_canonical()))
                    {
                        return Err(ValidationError {
                            path: format!("{path}/{direction}/rules/{index}/action"),
                            message: "community edits must be canonical sets",
                        });
                    }
                    if let RouteAction::Permit {
                        as_path_exclude: Some(asns),
                        ..
                    } = &rule.action
                        && asns.windows(2).any(|pair| pair[0] >= pair[1])
                    {
                        return Err(ValidationError {
                            path: format!(
                                "{path}/{direction}/rules/{index}/action/as_path_exclude"
                            ),
                            message: "AS path exclusions must be a canonical set",
                        });
                    }
                    if reflector
                        && neighbor.asn == self.local_as
                        && (matches!(
                            rule.action,
                            RouteAction::Permit {
                                preference: Some(_),
                                ..
                            } | RouteAction::Permit {
                                next_hop: Some(_),
                                ..
                            } | RouteAction::Permit {
                                origin: Some(_),
                                ..
                            } | RouteAction::Permit { med: Some(_), .. }
                                | RouteAction::Permit {
                                    as_path_replace: Some(_),
                                    ..
                                }
                        ) || matches!(&rule.action,
                            RouteAction::Permit { as_path_prepend: Some(asns), .. } if !asns.is_empty())
                            || matches!(&rule.action, RouteAction::Permit { as_path_exclude: Some(asns), .. } if !asns.is_empty()))
                    {
                        return Err(ValidationError {
                            path: format!("{path}/{direction}/rules/{index}/action"),
                            message: "reflector internal policy must preserve route attributes",
                        });
                    }
                    if let RouteAction::Permit {
                        next_hop: Some(address),
                        ..
                    } = rule.action
                        && (address.is_unspecified()
                            || address.is_multicast()
                            || matches!(address, IpAddr::V4(address) if address.is_broadcast())
                            || matches!(address, IpAddr::V6(address) if address.is_unicast_link_local() || address.to_ipv4_mapped().is_some()))
                    {
                        return Err(ValidationError {
                            path: format!("{path}/{direction}/rules/{index}/action/next_hop"),
                            message: "invalid policy next hop",
                        });
                    }
                }
            }
        }
        Ok(())
    }
    /// 本域的有效反射簇；客户端角色隐含启用反射，缺省簇身份等于本地 Identifier。
    /// 返回 None 表示普通 iBGP，全局没有另一份可写反射状态。
    pub fn reflection_cluster(&self) -> Option<Ipv4Addr> {
        self.cluster_id.or_else(|| {
            self.neighbors
                .values()
                .any(|neighbor| neighbor.route_reflector_client)
                .then_some(self.identifier)
        })
    }
    /// 将实际配置编码为唯一 JSON 内容；传输外壳由授权服务拥有。
    /// # Errors
    /// 非法配置或编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        self.validate().map_err(|_| Invalid)?;
        serde_json::to_vec(self).map_err(|_| Invalid)
    }
    /// 捕获并完整检查配置内容，不把坏输入当作空配置。
    /// # Errors
    /// 非法 JSON、未知字段或语义错误。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let result: Self = serde_json::from_slice(bytes).map_err(|_| Invalid)?;
        result.validate().map_err(|_| Invalid)?;
        Ok(result)
    }
}

/// 当前邻居的可观察状态；不是另一份可写会话状态。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeighborStatus {
    /// 稳定配置名，不是线程/PID。
    pub name: String,
    /// 当前配置的 transport 地址。
    pub address: IpAddr,
    /// 链路本地邻居的配置接口名；不是不稳定的 ifindex。
    pub interface: Option<Name>,
    /// 实际会话状态的标准 BGP 名称。
    pub state: String,
    /// 原始接收表条目，不表示已安装/可转发路由。
    pub received_routes: usize,
    /// 此邻居是否有显式入口策略；不等同于全部路由都被采用。
    pub import_policy_configured: bool,
    /// 此邻居是否配置出口策略。
    pub export_policy_configured: bool,
    /// 当前邻居是否要求 TCP MD5；不返回秘密或签名值。
    pub tcp_md5: bool,
    /// 此邻居配置的反射器客户端角色；尚未连接时仍可查看。
    pub route_reflector_client: bool,
    /// 当前连接已协商 IPv4 单播路由能力；未建立或未协商时为 false。
    pub ipv4_unicast: bool,
    /// 当前连接已协商 IPv6 单播路由能力；未建立或未协商时为 false。
    pub ipv6_unicast: bool,
    /// 已完整送入当前 TCP 的宣告数，不冒充对端确认或最终 FIB 安装量。
    pub advertised_routes: usize,
    /// 能力不相容后暂停此邻居；配置重建前不自动重新连接或接纳入站尝试。
    pub retry_blocked: bool,
    /// 当前失败观察；已有 Established 连接时不报告碰撞输家的错误。
    pub error: Option<String>,
}
/// 来自同一原生 actor 的完整会话观察；没有策略时明确报告未配置，不伪装已安装路由。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// 该实例采用的有效簇身份；None 为普通 iBGP。
    pub cluster_id: Option<Ipv4Addr>,
    /// 同一 actor 快照中的邻居观察。
    pub neighbors: Vec<NeighborStatus>,
    /// 已经实际来源确认的本地协议选择数，不把原始接收数当成安装数。
    pub selected_routes: usize,
    /// 有精确 RIB 路由依据的本机网段数，仍受每方向出口政策控制。
    pub originated_routes: usize,
    /// 来自其他实际 RIB 来源的可分发候选数，不冒充实际每邻居宣告或本域学习路由。
    pub redistributed_routes: usize,
    /// 实际接收/重选工作尚待确认，不维护可写的健康副本。
    pub routing_pending: bool,
}
impl Snapshot {
    /// 编码当前观察值。
    /// # Errors
    /// 编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        serde_json::to_vec(self).map_err(|_| Invalid)
    }
    /// 完整捕获状态正文。
    /// # Errors
    /// JSON 或字段非法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        serde_json::from_slice(bytes).map_err(|_| Invalid)
    }
}
