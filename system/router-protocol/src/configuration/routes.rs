//! 静态路由的持久意图；与声明正文分离，因为配置名不是运行时接口身份。
use super::*;
fn preference() -> u32 {
    1
}
fn weight() -> u32 {
    1
}

/// 静态 IPv4 前缀；同前缀可有不同偏好/下一跳候选，按原 RIB 规则裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StaticRoute {
    #[schemars(schema_with = "ipv4_schema")]
    pub destination: Ipv4Addr,
    #[schemars(range(max = 32))]
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
        #[schemars(schema_with = "optional_ipv4_schema")]
        gateway: Option<Ipv4Addr>,
        #[serde(default)]
        on_link: bool,
    },
    Recursive {
        #[schemars(schema_with = "ipv4_schema")]
        address: Ipv4Addr,
    },
}
fn optional_ipv4_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"anyOf":[{"type":"string","format":"ipv4"},{"type":"null"}]})
}
fn unicast(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
}
pub(super) fn validate(configuration: &Configuration) -> Result<(), ValidationError> {
    for (index, route) in configuration.routes.iter().enumerate() {
        let path = format!("/routes/{index}");
        let mask = u32::MAX
            .checked_shl(32 - u32::from(route.prefix_length.min(32)))
            .unwrap_or(0);
        if route.prefix_length > 32
            || route.preference == 0
            || u32::from(route.destination) & mask != u32::from(route.destination)
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
                        if !configuration.interfaces.contains_key(interface) {
                            return Err(invalid(
                                format!("{path}/target/interface"),
                                "unknown interface object",
                            ));
                        }
                        if *on_link && gateway.is_none()
                            || gateway.is_some_and(|gateway| !unicast(gateway))
                        {
                            return Err(invalid(
                                format!("{path}/target"),
                                "invalid direct gateway",
                            ));
                        }
                    }
                    StaticTarget::Recursive { address } if !unicast(*address) => {
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
/// `resolve` 对稳定设备匹配返回当前非零 ifindex；None 表示设备缺席，含糊匹配应返回 Invalid。
/// # Returns
/// 可捕获的完整 IPv4 来源；没有实际成员的转发组暂退，设备重现后重新投影持久声明。
/// # Errors
/// 未知引用、无效正文/数量、设备匹配含糊或内存不足；没有部分结果。
pub(super) fn compile(
    configuration: &Configuration,
    mut resolve: impl FnMut(&DeviceMatch) -> Result<Option<u32>, Invalid>,
) -> Result<Vec<u8>, crate::routing::EncodeError> {
    use crate::routing::{self, Action, Batch, Family, Member, NextHop, Target};
    let declarations = &configuration.routes;
    let mut attributes = Vec::new();
    let mut paths = Vec::new();
    let mut routes = Vec::new();
    u32::try_from(declarations.len()).map_err(|_| routing::EncodeError::Invalid)?;
    attributes
        .try_reserve_exact(declarations.len())
        .map_err(|_| routing::EncodeError::NoMemory)?;
    paths
        .try_reserve_exact(declarations.len())
        .map_err(|_| routing::EncodeError::NoMemory)?;
    routes
        .try_reserve_exact(declarations.len())
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
                        StaticTarget::Recursive { address } => Target::Recursive((*address).into()),
                        StaticTarget::Direct {
                            interface,
                            gateway,
                            on_link,
                        } => {
                            let profile = configuration
                                .interfaces
                                .get(interface)
                                .ok_or(routing::EncodeError::Invalid)?;
                            let Some(index) = resolve(&profile.device)
                                .map_err(|_| routing::EncodeError::Invalid)?
                            else {
                                continue;
                            };
                            Target::Direct {
                                interface_index: index,
                                gateway: gateway.map(Into::into),
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
            network: declaration.destination.into(),
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
    Batch::encode(Family::Ipv4, &attributes, &hops, &routes)
}
