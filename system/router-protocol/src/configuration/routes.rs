//! 静态路由的持久意图；与声明正文分离，因为配置名不是运行时接口身份。
use super::*;
use core::net::IpAddr;
fn preference() -> u32 {
    1
}
fn weight() -> u32 {
    1
}

/// 静态组播路径。入口执行反向路径检查，出口列表限制实际复制范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MulticastRoute {
    #[schemars(schema_with = "address_schema")]
    pub group: IpAddr,
    #[serde(default)]
    #[schemars(schema_with = "optional_address_schema")]
    pub source: Option<IpAddr>,
    pub ingress: Name,
    pub outputs: Vec<Name>,
}

impl Configuration {
    /// 解析具名组播接口。设备缺席时保留精确源规则的优先级。
    /// # Errors
    /// 引用、重复绑定或分配无效。
    pub fn multicast_source(
        &self,
        resolve: impl Fn(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, crate::routing::EncodeError> {
        use crate::{
            multicast::{self, Route},
            routing::EncodeError,
        };
        let mut routes = Vec::new();
        routes
            .try_reserve_exact(self.multicast_routes.len())
            .map_err(|_| EncodeError::NoMemory)?;
        let index = |name| {
            let reference = self.interface_reference(name).ok_or(EncodeError::Invalid)?;
            resolve(reference).map_err(|_| EncodeError::Invalid)
        };
        for route in &self.multicast_routes {
            let ingress = index(&route.ingress)?.unwrap_or(0);
            let mut outputs = Vec::new();
            outputs
                .try_reserve_exact(route.outputs.len())
                .map_err(|_| EncodeError::NoMemory)?;
            for name in &route.outputs {
                outputs.extend(index(name)?);
            }
            outputs.sort_unstable();
            routes.push(Route {
                key: (route.group, route.source),
                ingress,
                outputs,
            });
        }
        routes.sort_unstable_by_key(|route| route.key);
        multicast::encode(&routes)
    }
}

pub(super) fn validate_multicast(configuration: &Configuration) -> Result<(), ValidationError> {
    use crate::multicast::{MAX_OUTPUTS, MAX_ROUTES, valid_key};
    if configuration.multicast_routes.len() > MAX_ROUTES {
        return Err(invalid("/multicast_routes", "too many multicast routes"));
    }
    let mut keys = BTreeSet::new();
    let usable = |name: &Name| {
        configuration.interface_reference(name).is_some()
            && !configuration
                .bridges
                .values()
                .any(|bridge| bridge.ports.contains_key(name))
    };
    for (index, route) in configuration.multicast_routes.iter().enumerate() {
        let key = (route.group, route.source);
        if !valid_key(key)
            || !keys.insert(key)
            || !usable(&route.ingress)
            || !(1..=MAX_OUTPUTS).contains(&route.outputs.len())
            || route.outputs.iter().enumerate().any(|(i, name)| {
                name == &route.ingress || !usable(name) || route.outputs[..i].contains(name)
            })
        {
            return Err(invalid(
                format!("/multicast_routes/{index}"),
                "invalid multicast group, source or routed interface list",
            ));
        }
    }
    Ok(())
}

/// 静态双栈前缀；同前缀可有不同偏好/下一跳候选，按原 RIB 规则裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StaticRoute {
    #[schemars(schema_with = "address_schema")]
    pub destination: IpAddr,
    /// 目标前缀位数：IPv4 为 0..=32，IPv6 为 0..=128。
    #[schemars(range(max = 128))]
    pub prefix_length: u8,
    /// 较小优先；0 留给实际接口事实。
    #[serde(default = "preference")]
    #[schemars(range(min = 1))]
    pub preference: u32,
    #[serde(default)]
    pub metric: u32,
    pub action: StaticAction,
}
/// 转发与显式终止动作；终止不带无效的出口字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StaticAction {
    Forward { members: Vec<StaticMember> },
    Discard,
    Unreachable,
    Prohibit,
}
/// 带权路径；权重交由来源捕获做精确比例规范化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StaticMember {
    #[serde(default = "weight")]
    #[schemars(range(min = 1))]
    pub weight: u32,
    pub target: StaticTarget,
}
/// 下一跳的持久引用；直接接口必须是本配置树的具名对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StaticTarget {
    Direct {
        interface: Name,
        #[serde(default)]
        #[schemars(schema_with = "optional_address_schema")]
        gateway: Option<IpAddr>,
        #[serde(default)]
        on_link: bool,
    },
    Recursive {
        #[schemars(schema_with = "address_schema")]
        address: IpAddr,
    },
}
fn unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
        }
        IpAddr::V6(address) => {
            !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_loopback()
                && address.to_ipv4_mapped().is_none()
        }
    }
}
pub(super) fn validate(
    routes: &[StaticRoute],
    section: &str,
    interface_exists: impl Fn(&Name) -> bool,
) -> Result<(), ValidationError> {
    for (index, route) in routes.iter().enumerate() {
        let path = format!("{section}/{index}");
        let (width, network) = match route.destination {
            IpAddr::V4(address) => (32, u128::from(u32::from(address))),
            IpAddr::V6(address) => (128, u128::from(address)),
        };
        let mask = u128::MAX
            .checked_shl(width - u32::from(route.prefix_length).min(width))
            .unwrap_or(0);
        if u32::from(route.prefix_length) > width
            || route.preference == 0
            || network & mask != network
        {
            return Err(invalid(path, "invalid canonical prefix or preference"));
        }
        if let StaticAction::Forward { members } = &route.action {
            if members.is_empty() {
                return Err(invalid(
                    format!("{path}/action/members"),
                    "forwarding group is empty",
                ));
            }
            for (position, member) in members.iter().enumerate() {
                let path = format!("{path}/action/members/{position}");
                if member.weight == 0
                    || members[..position]
                        .iter()
                        .any(|previous| previous.target == member.target)
                {
                    return Err(invalid(path, "zero weight or duplicate target"));
                }
                match &member.target {
                    StaticTarget::Direct {
                        interface,
                        gateway,
                        on_link,
                    } => {
                        if !interface_exists(interface) {
                            return Err(invalid(
                                format!("{path}/target/interface"),
                                "unknown interface object",
                            ));
                        }
                        if *on_link && gateway.is_none()
                            || gateway.is_some_and(|gateway| {
                                !unicast(gateway)
                                    || gateway.is_ipv4() && route.destination.is_ipv6()
                            })
                        {
                            return Err(invalid(
                                format!("{path}/target"),
                                "invalid direct gateway",
                            ));
                        }
                    }
                    StaticTarget::Recursive { address }
                        if !unicast(*address)
                            || address.is_ipv4() && route.destination.is_ipv6()
                            || matches!(address, IpAddr::V6(address) if address.is_unicast_link_local()) =>
                    {
                        return Err(invalid(
                            format!("{path}/target/address"),
                            "invalid recursive gateway",
                        ));
                    }
                    StaticTarget::Recursive { .. } => {}
                }
            }
        }
    }
    Ok(())
}

/// 将已校验的静态配置投影为唯一来源正文；实际设备绑定由配置服务提供，不写入配置树。
/// # Parameters
/// `reference` 只解析当前路由域内的接口配置名。`resolve` 将稳定引用解析到当前 ifindex；缺席返回 None，含糊返回 Invalid。
/// # Returns
/// 可捕获的指定地址族完整来源；没有实际成员的转发组暂退，设备重现后重新投影持久声明。
/// # Errors
/// 未知引用、无效正文/数量、设备匹配含糊或内存不足；没有部分结果。
pub(super) fn compile<'a>(
    declared: &'a [StaticRoute],
    family: crate::routing::Family,
    reference: impl Fn(&'a Name) -> Option<InterfaceReference<'a>>,
    mut resolve: impl FnMut(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
) -> Result<Vec<u8>, crate::routing::EncodeError> {
    use crate::routing::{self, Action, Batch, Family, Member, NextHop, Target};
    let declarations = declared.iter().filter(|route| {
        matches!(
            (family, route.destination),
            (Family::Ipv4, IpAddr::V4(_)) | (Family::Ipv6, IpAddr::V6(_))
        )
    });
    let count = declarations.clone().count();
    let mut attributes = Vec::new();
    let mut paths = Vec::new();
    let mut routes = Vec::new();
    u32::try_from(count).map_err(|_| routing::EncodeError::Invalid)?;
    attributes
        .try_reserve_exact(count)
        .map_err(|_| routing::EncodeError::NoMemory)?;
    paths
        .try_reserve_exact(count)
        .map_err(|_| routing::EncodeError::NoMemory)?;
    routes
        .try_reserve_exact(count)
        .map_err(|_| routing::EncodeError::NoMemory)?;
    for declaration in declarations {
        let mut members = Vec::new();
        let action = match &declaration.action {
            StaticAction::Discard => Action::Discard,
            StaticAction::Unreachable => Action::Unreachable,
            StaticAction::Prohibit => Action::Prohibit,
            StaticAction::Forward { members: declared } => {
                members
                    .try_reserve_exact(declared.len())
                    .map_err(|_| routing::EncodeError::NoMemory)?;
                for member in declared {
                    let target = match &member.target {
                        StaticTarget::Recursive { address } => Target::Recursive(*address),
                        StaticTarget::Direct {
                            interface,
                            gateway,
                            on_link,
                        } => {
                            let reference =
                                reference(interface).ok_or(routing::EncodeError::Invalid)?;
                            let Some(index) =
                                resolve(reference).map_err(|_| routing::EncodeError::Invalid)?
                            else {
                                continue;
                            };
                            Target::Direct {
                                interface_index: index,
                                gateway: *gateway,
                                on_link: *on_link,
                            }
                        }
                    };
                    members.push(Member {
                        target,
                        weight: member.weight,
                    });
                }
                // 无硬件时保留持久意图，暂不声明空转发组；设备安装走同一路径重新投影全部来源。
                if members.is_empty() {
                    continue;
                }
                Action::Forward
            }
        };
        let ordinal = routes.len() as u32;
        attributes.push(routing::Attributes {
            preference: declaration.preference,
            metric: declaration.metric,
        });
        paths.push((action, members));
        routes.push(routing::Route {
            network: declaration.destination,
            length: declaration.prefix_length,
            attributes: ordinal,
            next_hop: ordinal,
        });
    }
    let mut hops = Vec::new();
    hops.try_reserve_exact(paths.len())
        .map_err(|_| routing::EncodeError::NoMemory)?;
    hops.extend(
        paths
            .iter()
            .map(|(action, members)| NextHop::new(*action, members)),
    );
    Batch::encode(family, &attributes, &hops, &routes)
}
