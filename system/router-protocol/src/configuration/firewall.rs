//! 预路由规则用稳定接口配置名声明，运行时身份来自现有设备绑定。

use super::*;
pub use crate::firewall::ConnectionState;
use crate::tracking::IpPrefix;
use crate::{
    firewall::{self, ExistingConnections},
    routing::EncodeError,
};
use core::net::IpAddr;

/// 持久规则动作，跳转目标使用配置中的子链名称。
pub type PreroutingAction = firewall::PreroutingAction<Name>;

/// 双栈防火墙的持久规则链。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Firewall {
    /// 发布新规则时，是否让已有的转发连接沿用此前的规则。
    #[serde(default, skip_serializing_if = "reevaluates_connections")]
    pub existing_connections: ExistingConnections,
    /// 可由多条规则引用的双栈目的网段集合。
    #[serde(
        default,
        deserialize_with = "unique_objects",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub address_sets: BTreeMap<Name, Vec<IpPrefix>>,
    /// 按配置顺序处理入站转发流量。
    #[serde(default)]
    pub prerouting: Vec<PreroutingRule>,
    /// 由入口链或其他子链调用的有序规则组。
    #[serde(
        default,
        deserialize_with = "unique_objects",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub chains: BTreeMap<Name, Vec<PreroutingRule>>,
}

fn reevaluates_connections(mode: &ExistingConnections) -> bool {
    *mode == ExistingConnections::Reevaluate
}

impl Firewall {
    pub(super) fn is_empty(&self) -> bool {
        reevaluates_connections(&self.existing_connections)
            && self.prerouting.is_empty()
            && self.address_sets.is_empty()
            && self.chains.is_empty()
    }

    /// 依次遍历入口链及按名排列的子链规则。
    pub fn rules(&self) -> impl Iterator<Item = &PreroutingRule> + Clone {
        self.prerouting.iter().chain(self.chains.values().flatten())
    }

    fn chain_names(&self) -> Result<Vec<&Name>, EncodeError> {
        let mut names = Vec::new();
        names
            .try_reserve_exact(self.chains.len())
            .map_err(|_| EncodeError::NoMemory)?;
        names.extend(self.chains.keys());
        Ok(names)
    }
}

/// 在确定转发出口前按入接口、源和目的网段匹配的规则。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("oneOf" = [
    {"required": ["destination", "prefix_length"], "not": {"required": ["destination_set"]}},
    {"required": ["destination_set"], "not": {"anyOf": [
        {"required": ["destination"]}, {"required": ["prefix_length"]}
    ]}},
    {"not": {"anyOf": [
        {"required": ["destination"]}, {"required": ["prefix_length"]}, {"required": ["destination_set"]}
    ]}}
]))]
pub struct PreroutingRule {
    /// 缺省匹配所有入接口。
    pub ingress: Option<Name>,
    /// 缺省匹配两族的任意来源。指定时只匹配该地址族。
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub source: Option<IpPrefix>,
    /// 直接指定目的网段时的规范网络地址。
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(schema_with = "address_schema")]
    pub destination: Option<IpAddr>,
    /// 与 destination 一起指定的前缀位数。
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "u8", range(max = 128))]
    pub prefix_length: Option<u8>,
    /// 直接目的网段的替代匹配条件，引用已声明的地址集。
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Name")]
    pub destination_set: Option<Name>,
    pub action: PreroutingAction,
    /// 可选连接状态；省略时匹配所有状态及未分类报文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_state: Option<ConnectionState>,
    /// 可选 TCP 或 UDP 目的服务。
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub transport: Option<firewall::TransportMatch>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    decoder: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(decoder).map(Some)
}

impl PreroutingRule {
    /// 校验转换地址、端口与本规则的匹配条件。子链、接口和地址集引用由整份配置校验。
    pub fn valid_translation(&self) -> bool {
        self.destination(|_| Some(0)).is_some_and(|destination| {
            self.action
                .translation_valid(self.source, destination, self.transport)
        })
    }

    fn destination(
        &self,
        set_index: impl FnOnce(&Name) -> Option<u32>,
    ) -> Option<firewall::Destination> {
        match (self.destination, self.prefix_length, &self.destination_set) {
            (Some(network), Some(length), None) => {
                let prefix = IpPrefix { network, length };
                firewall::valid_prefix(prefix).then_some(firewall::Destination::Prefix(prefix))
            }
            (None, None, Some(name)) => set_index(name).map(firewall::Destination::Set),
            (None, None, None) => Some(firewall::Destination::Any),
            _ => None,
        }
    }
}

fn chain_index(names: &[&Name], name: &Name) -> Option<u32> {
    u32::try_from(names.binary_search(&name).ok()?.checked_add(1)?).ok()
}

impl Firewall {
    pub(super) fn validate(
        &self,
        interface_exists: impl Fn(&Name) -> bool,
    ) -> Result<(), ValidationError> {
        for (name, members) in &self.address_sets {
            let key = name.as_str().replace('~', "~0").replace('/', "~1");
            if !firewall::valid_name(name.as_str()) {
                return Err(invalid(
                    format!("/firewall/address_sets/{key}"),
                    "invalid address set name",
                ));
            }
            if let Some(index) = members
                .iter()
                .position(|prefix| !firewall::valid_prefix(*prefix))
            {
                return Err(invalid(
                    format!("/firewall/address_sets/{key}/{index}"),
                    "invalid address set prefix",
                ));
            }
        }
        let chains = core::iter::once((None, &self.prerouting))
            .chain(self.chains.iter().map(|(name, rules)| (Some(name), rules)));
        for (name, rules) in chains.clone() {
            let path = || match name {
                Some(name) => format!(
                    "/firewall/chains/{}",
                    name.as_str().replace('~', "~0").replace('/', "~1")
                ),
                None => String::from("/firewall/prerouting"),
            };
            if name.is_some_and(|name| !firewall::valid_chain_name(name.as_str())) {
                return Err(invalid(path(), "invalid firewall chain name"));
            }
            for (index, rule) in rules.iter().enumerate() {
                let Some(destination) =
                    rule.destination(|name| self.address_sets.contains_key(name).then_some(0))
                else {
                    return Err(invalid(
                        format!("{}/{index}/destination", path()),
                        "invalid prefix or address set selector",
                    ));
                };
                if rule.source.is_some_and(|source| {
                    !firewall::valid_prefix(source)
                        || matches!(destination, firewall::Destination::Prefix(prefix) if prefix.network.is_ipv6() != source.network.is_ipv6())
                }) {
                    return Err(invalid(
                        format!("{}/{index}/source", path()),
                        "invalid source prefix or address family mismatch",
                    ));
                }
                if !rule
                    .action
                    .translation_valid(rule.source, destination, rule.transport)
                {
                    return Err(invalid(
                        format!("{}/{index}/action", path()),
                        "invalid NAT address, family mismatch or port translation without TCP/UDP match",
                    ));
                }
                if rule
                    .ingress
                    .as_ref()
                    .is_some_and(|name| !interface_exists(name))
                {
                    return Err(invalid(
                        format!("{}/{index}/ingress", path()),
                        "unknown ingress interface",
                    ));
                }
                if matches!(&rule.action, PreroutingAction::Jump { chain } if !self.chains.contains_key(chain))
                {
                    return Err(invalid(
                        format!("{}/{index}/action/chain", path()),
                        "unknown firewall chain",
                    ));
                }
            }
        }
        if !self.chains.is_empty() {
            let names = self.chain_names().map_err(|_| {
                invalid(
                    "/firewall/chains",
                    "insufficient memory for chain validation",
                )
            })?;
            firewall::check_jump_graph(
                names.len() + 1,
                chains.map(|(_, rules)| {
                    let names = &names;
                    rules.iter().filter_map(move |rule| match &rule.action {
                        PreroutingAction::Jump { chain } => {
                            Some(chain_index(names, chain).unwrap_or(0))
                        }
                        _ => None,
                    })
                }),
            )
            .map_err(|error| {
                invalid(
                    "/firewall/chains",
                    match error {
                        EncodeError::Invalid => "firewall chain loop or nesting limit exceeded",
                        EncodeError::NoMemory => "insufficient memory for chain validation",
                    },
                )
            })?;
        }
        Ok(())
    }

    pub(super) fn compile<'a>(
        &'a self,
        interface: impl Fn(&'a Name) -> Option<InterfaceReference<'a>>,
        resolve: impl Fn(InterfaceReference<'a>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        let rules = self.rules();
        let chain_names = self.chain_names()?;
        let mut names = Vec::new();
        for name in rules
            .clone()
            .filter_map(|rule| rule.destination_set.as_ref())
        {
            names.try_reserve(1).map_err(|_| EncodeError::NoMemory)?;
            names.push(name);
        }
        names.sort_unstable();
        names.dedup();
        let sets = names.iter().map(|name| {
            let members = self.address_sets.get(*name).ok_or(EncodeError::Invalid)?;
            Ok((name.as_str(), members.as_slice()))
        });
        firewall::encode(
            self.existing_connections,
            rules.clone().count(),
            self.prerouting.len(),
            self.chains
                .iter()
                .map(|(name, rules)| (name.as_str(), rules.len())),
            rules.map(|rule| {
                let destination = rule
                    .destination(|name| u32::try_from(names.binary_search(&name).ok()?).ok())
                    .ok_or(EncodeError::Invalid)?;
                let ingress = if let Some(name) = &rule.ingress {
                    let reference = interface(name).ok_or(EncodeError::Invalid)?;
                    match resolve(reference) {
                        Ok(Some(index)) if index > crate::interface::LOOPBACK_INDEX => {
                            firewall::Ingress::Interface(index)
                        }
                        Ok(None) => firewall::Ingress::Unavailable,
                        _ => return Err(EncodeError::Invalid),
                    }
                } else {
                    firewall::Ingress::Any
                };
                let action = match &rule.action {
                    PreroutingAction::SetMark { value } => {
                        firewall::PreroutingAction::SetMark { value: *value }
                    }
                    PreroutingAction::Accept => firewall::PreroutingAction::Accept,
                    PreroutingAction::Drop => firewall::PreroutingAction::Drop,
                    PreroutingAction::Return => firewall::PreroutingAction::Return,
                    PreroutingAction::DestinationNat {
                        address,
                        port,
                        source,
                    } => firewall::PreroutingAction::DestinationNat {
                        address: *address,
                        port: *port,
                        source: *source,
                    },
                    PreroutingAction::SourceNat(target) => {
                        firewall::PreroutingAction::SourceNat(*target)
                    }
                    PreroutingAction::Masquerade => firewall::PreroutingAction::Masquerade,
                    PreroutingAction::Jump { chain } => firewall::PreroutingAction::Jump {
                        chain: chain_index(&chain_names, chain).ok_or(EncodeError::Invalid)?,
                    },
                };
                Ok(firewall::Rule {
                    ingress,
                    source: rule.source,
                    destination,
                    action,
                    connection_state: rule.connection_state,
                    transport: rule.transport,
                })
            }),
            sets,
        )
    }
}

impl Configuration {
    /// 投影默认域的防火墙规则。`resolve` 提供设备身份，暂缺设备的规则随设备重新出现而恢复。
    /// # Errors
    /// 接口引用、前缀或资源容量非法。
    pub fn firewall_source(
        &self,
        resolve: impl Fn(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        self.firewall
            .compile(|name| self.interface_reference(name), resolve)
    }
}
