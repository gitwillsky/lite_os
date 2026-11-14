//! 持久选表配置。入接口引用现有具名接口，运行时索引由当前设备投影提供。

use super::*;
use crate::{
    policy_routing::{self, Rule, alternate_table},
    routing::{EncodeError, Family},
};
use core::net::IpAddr;

/// 备选路由表与按优先级匹配的选表规则。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyRouting {
    #[serde(default)]
    pub tables: Vec<StaticTable>,
    #[serde(default)]
    pub rules: Vec<PolicyRouteRule>,
}

/// 备选表可同时包含 IPv4 和 IPv6 静态路由，也可为空。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StaticTable {
    pub id: u32,
    #[serde(default)]
    pub routes: Vec<StaticRoute>,
}

/// 配置流量的选表条件。优先级数字较小的规则先匹配。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyRouteRule {
    #[schemars(range(min = 1, max = 32765))]
    pub priority: u32,
    #[schemars(schema_with = "address_schema")]
    pub source: IpAddr,
    #[schemars(range(max = 128))]
    pub prefix_length: u8,
    #[serde(default)]
    pub ingress: Option<Name>,
    /// 不指定时继续匹配跨接口转发流量。
    #[serde(default)]
    pub origin: RuleOrigin,
    /// 可选报文标记，省略时匹配任意标记。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<u32>,
    /// 比较位，省略时比较指定标记的全部位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark_mask: Option<u32>,
    pub table: u32,
}

/// 本机流量以回环身份选表，跨接口转发以实际入接口选表。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleOrigin {
    #[default]
    Forwarded,
    Local,
}

impl PolicyRouteRule {
    fn project(&self, ingress: u32) -> Rule {
        Rule {
            priority: self.priority,
            source: self.source,
            prefix_length: self.prefix_length,
            ingress,
            table: self.table,
            mark: self.mark.unwrap_or(0),
            mark_mask: self.mark.map_or(0, |_| self.mark_mask.unwrap_or(u32::MAX)),
        }
    }
}

impl PolicyRouting {
    pub(super) fn validate(
        &self,
        interface_exists: impl Fn(&Name) -> bool,
    ) -> Result<(), ValidationError> {
        let mut tables = BTreeSet::new();
        for (index, table) in self.tables.iter().enumerate() {
            if !alternate_table(table.id) || !tables.insert(table.id) {
                return Err(invalid(
                    format!("/policy_routing/tables/{index}/id"),
                    "invalid or duplicate table ID",
                ));
            }
            routes::validate(
                &table.routes,
                &format!("/policy_routing/tables/{index}/routes"),
                &interface_exists,
            )?;
        }
        let mut priorities = BTreeSet::new();
        for (index, rule) in self.rules.iter().enumerate() {
            if let Some(mask) = rule.mark_mask
                && (mask == 0 || rule.mark.is_none_or(|mark| mark & !mask != 0))
            {
                return Err(invalid(
                    format!("/policy_routing/rules/{index}/mark_mask"),
                    "nonzero mask requires a mark within the selected bits",
                ));
            }
            if !rule.project(0).valid() || !priorities.insert(rule.priority) {
                return Err(invalid(
                    format!("/policy_routing/rules/{index}"),
                    "invalid source prefix or rule priority",
                ));
            }
            if !tables.contains(&rule.table) {
                return Err(invalid(
                    format!("/policy_routing/rules/{index}/table"),
                    "unknown routing table",
                ));
            }
            if rule.origin == RuleOrigin::Local && rule.ingress.is_some() {
                return Err(invalid(
                    format!("/policy_routing/rules/{index}/ingress"),
                    "locally originated traffic has no physical ingress interface",
                ));
            }
            if rule
                .ingress
                .as_ref()
                .is_some_and(|name| !interface_exists(name))
            {
                return Err(invalid(
                    format!("/policy_routing/rules/{index}/ingress"),
                    "unknown interface profile",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn compile<'a>(
        &'a self,
        interface: impl Fn(&'a Name) -> Option<InterfaceReference<'a>>,
        mut resolve: impl FnMut(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(self.tables.len())
            .map_err(|_| EncodeError::NoMemory)?;
        for table in &self.tables {
            tables.push((
                table.id,
                [
                    routes::compile(&table.routes, Family::Ipv4, &interface, &mut resolve)?,
                    routes::compile(&table.routes, Family::Ipv6, &interface, &mut resolve)?,
                ],
            ));
        }
        tables.sort_unstable_by_key(|(id, _)| *id);
        let mut rules = Vec::new();
        rules
            .try_reserve_exact(self.rules.len())
            .map_err(|_| EncodeError::NoMemory)?;
        for rule in &self.rules {
            let ingress = match (rule.origin, &rule.ingress) {
                (RuleOrigin::Local, None) => crate::interface::LOOPBACK_INDEX,
                (RuleOrigin::Local, Some(_)) => return Err(EncodeError::Invalid),
                (RuleOrigin::Forwarded, None) => 0,
                (RuleOrigin::Forwarded, Some(name)) => {
                    let reference = interface(name).ok_or(EncodeError::Invalid)?;
                    match resolve(reference).map_err(|_| EncodeError::Invalid)? {
                        Some(0 | crate::interface::LOOPBACK_INDEX) => {
                            return Err(EncodeError::Invalid);
                        }
                        Some(index) => index,
                        None => continue,
                    }
                }
            };
            rules.push(rule.project(ingress));
        }
        rules.sort_unstable_by_key(|rule| rule.priority);
        policy_routing::encode(&rules, &tables)
    }
}

impl Configuration {
    /// 投影默认域的备选表及规则。`resolve` 解析当前设备，缺席设备的成员及规则暂退。
    /// # Errors
    /// 未知接口、绑定含糊、表示超限或内存不足。
    pub fn policy_source(
        &self,
        resolve: impl FnMut(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        self.policy_routing
            .compile(|name| self.interface_reference(name), resolve)
    }
}
