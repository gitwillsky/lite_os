//! 持久路由域的接口归属和本域选路意图。

use super::*;
use crate::routing::{EncodeError, Family};

const ROUTING_DOMAIN_NAME_MAX: usize = 255;

/// 返回 `name` 是否可作为持久路由域名称，限 1–255 UTF-8 字节且无控制字符。
pub fn valid_routing_domain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= ROUTING_DOMAIN_NAME_MAX && !name.chars().any(char::is_control)
}

/// 独立管理的双栈路由域。接口、路由表和规则引用都在本域内解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutingDomain {
    /// 按配置名称索引的接口，使用 MAC 确定物理归属。
    #[serde(default, deserialize_with = "unique_objects")]
    pub interfaces: BTreeMap<Name, Interface>,
    /// 本域的双栈静态路由。
    #[serde(default)]
    pub routes: Vec<StaticRoute>,
    /// 本域的备选路由表与选表规则。
    #[serde(default)]
    pub policy_routing: PolicyRouting,
    /// 本域的转发防火墙、连接处理策略和 NAT 规则。
    #[serde(default, skip_serializing_if = "Firewall::is_empty")]
    pub firewall: Firewall,
    /// 本域受管路由的准入策略。
    #[serde(default)]
    pub route_policy: crate::policy::Policy,
    /// 本域的 BGP 邻居和收发策略。链路本地邻居引用本域接口配置名称。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<crate::bgp::Configuration>,
    /// 本域的 OSPFv2/v3 路由器及链路配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ospf: Option<crate::ospf::Configuration>,
    /// 本域是否允许 IPv4 转发。
    #[serde(default = "forwarding_default")]
    pub ipv4_forwarding: bool,
    /// 本域是否允许 IPv6 转发。
    #[serde(default = "forwarding_default")]
    pub ipv6_forwarding: bool,
}

impl Default for RoutingDomain {
    fn default() -> Self {
        Self {
            interfaces: Default::default(),
            routes: Vec::new(),
            policy_routing: Default::default(),
            firewall: Default::default(),
            route_policy: Default::default(),
            bgp: None,
            ospf: None,
            ipv4_forwarding: forwarding_default(),
            ipv6_forwarding: forwarding_default(),
        }
    }
}

impl RoutingDomain {
    fn interface_reference(&self, name: &Name) -> Option<InterfaceReference<'_>> {
        self.interfaces
            .get(name)
            .map(|row| InterfaceReference::Device(&row.device))
    }

    /// 捕获本域 OSPF 会话配置，运行时接口按本域设备身份匹配。
    /// # Errors
    /// 链路配置或本域接口引用无效。
    pub fn ospf_session(&self) -> Result<Option<crate::ospf::Session>, ValidationError> {
        self.ospf
            .as_ref()
            .map(|ospf| {
                ospf.project_links([self.ipv4_forwarding, self.ipv6_forwarding], |name| {
                    self.interface_reference(name)
                })
            })
            .transpose()
    }

    /// 编译 `family` 的静态来源。`resolve` 只查询本域的当前接口；缺席接口暂不安装。
    /// # Errors
    /// 接口引用、路由正文无效，或分配失败。
    pub fn route_source(
        &self,
        family: Family,
        resolve: impl FnMut(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        routes::compile(
            &self.routes,
            family,
            |name| self.interface_reference(name),
            resolve,
        )
    }

    /// 编译本域的备选表及选表规则。`resolve` 查询本域设备身份。
    /// # Errors
    /// 未知接口、绑定含糊、表示超限或内存不足。
    pub fn policy_source(
        &self,
        resolve: impl FnMut(InterfaceReference<'_>) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        self.policy_routing
            .compile(|name| self.interface_reference(name), resolve)
    }

    /// 编译本域的防火墙规则。`resolve` 查询本域设备身份，暂缺设备的规则保持停用。
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

impl Configuration {
    /// 返回当前有效配置中由 `mac` 绑定的具名域。返回 None 表示默认域。
    pub fn routing_domain_for_mac(&self, mac: [u8; 6]) -> Option<&Name> {
        let device = DeviceMatch::Mac(mac);
        self.routing_domains.iter().find_map(|(name, domain)| {
            domain
                .interfaces
                .values()
                .any(|interface| interface.device == device)
                .then_some(name)
        })
    }

    pub(super) fn validate_routing_domains(&self) -> Result<(), ValidationError> {
        let mut placed = BTreeSet::new();
        for (name, domain) in &self.routing_domains {
            let path = format!(
                "/routing_domains/{}",
                name.as_str().replace('~', "~0").replace('/', "~1")
            );
            if !valid_routing_domain_name(name.as_str()) {
                return Err(invalid(path, "routing domain name exceeds 255 UTF-8 bytes"));
            }
            let at_domain = |mut error: ValidationError| {
                error.path = format!("{path}{}", error.path);
                error
            };
            validate_interfaces(&domain.interfaces).map_err(at_domain)?;
            routes::validate(&domain.routes, "/routes", |name| {
                domain.interfaces.contains_key(name)
            })
            .map_err(at_domain)?;
            let interface_exists = |name: &Name| domain.interfaces.contains_key(name);
            domain
                .policy_routing
                .validate(interface_exists)
                .map_err(at_domain)?;
            domain
                .firewall
                .validate(interface_exists)
                .map_err(at_domain)?;
            validate_route_policy(&domain.route_policy).map_err(at_domain)?;
            if let Some(ospf) = &domain.ospf {
                ospf.validate_links(interface_exists).map_err(at_domain)?;
            }
            if let Some(bgp) = &domain.bgp {
                bgp.validate().map_err(at_domain)?;
                for (name, neighbor) in &bgp.neighbors {
                    if neighbor
                        .interface
                        .as_ref()
                        .is_some_and(|name| !interface_exists(name))
                    {
                        return Err(invalid(
                            format!(
                                "{path}/bgp/neighbors/{}/interface",
                                name.as_str().replace('~', "~0").replace('/', "~1")
                            ),
                            "BGP interface must belong to this routing domain",
                        ));
                    }
                }
            }
            for (name, interface) in &domain.interfaces {
                let path = format!(
                    "{path}/interfaces/{}",
                    name.as_str().replace('~', "~0").replace('/', "~1")
                );
                let DeviceMatch::Mac(mac) = interface.device else {
                    return Err(invalid(
                        format!("{path}/device"),
                        "routing domain requires a MAC binding",
                    ));
                };
                if !placed.insert(mac) {
                    return Err(invalid(
                        format!("{path}/device"),
                        "device belongs to multiple routing domains",
                    ));
                }
                if interface.ipv4.as_ref().is_some_and(|ipv4| {
                    ipv4.assignment != Assignment::Static
                        || !ipv4.dns.is_empty()
                        || !ipv4.dns_suffix.is_empty()
                }) || interface.dhcp_v4_server.is_some()
                {
                    return Err(invalid(
                        path,
                        "routing domain supports static IPv4 without DHCP or resolver settings",
                    ));
                }
            }
        }
        if !placed.is_empty() {
            for (name, interface) in &self.interfaces {
                let issue = match interface.device {
                    DeviceMatch::Mac(mac) if placed.contains(&mac) => {
                        "device belongs to multiple routing domains"
                    }
                    // 跨域安装改变各域的运行时 ethN 枚举，旧名字不能用来重放同一物理绑定。
                    DeviceMatch::Name(_) => {
                        "default domain interfaces require MAC bindings when devices are assigned to routing domains"
                    }
                    _ => continue,
                };
                return Err(invalid(
                    format!(
                        "/interfaces/{}/device",
                        name.as_str().replace('~', "~0").replace('/', "~1")
                    ),
                    issue,
                ));
            }
        }
        Ok(())
    }
}
