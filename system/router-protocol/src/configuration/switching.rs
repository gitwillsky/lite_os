//! 网桥端口由现有接口配置名指定；仅在投影到 NET 时解析实际设备身份。
use super::{
    BTreeMap, BTreeSet, Configuration, Interface, InterfaceReference, Name, ValidationError,
    invalid, unique_objects,
};
use crate::{
    Invalid,
    dhcp_server::Pool,
    routing::EncodeError,
    switching::{MAX_PORTS, valid_vid},
};
use alloc::{format, vec::Vec};
use core::net::Ipv4Addr;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 端口所属 VLAN 的出站封装。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VlanEgress {
    Tagged,
    Untagged,
}

/// 端口允许的 VLAN、缺省入口 VID 和端口隔离。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BridgePort {
    /// 无标签及 priority tag 的入口 VID；缺席拒绝这两种帧。
    #[serde(default)]
    pub pvid: Option<u16>,
    /// 端口准入的 VID 到出站封装的映射。
    #[serde(deserialize_with = "unique_objects")]
    pub vlans: BTreeMap<u16, VlanEgress>,
    /// 隔离端口之间不交换流量，仍可到达普通端口。
    #[serde(default)]
    pub isolated: bool,
    /// 快速启用仅连接终端的端口。收到 BPDU 后撤销边缘资格。
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub edge: bool,
    /// 独占全双工链路允许 proposal/agreement。共享链路使用转发时钟。
    #[serde(default = "rstp_default")]
    pub point_to_point: bool,
    /// IEEE 长路径代价。缺省使用千兆以太网的代价。
    #[serde(default = "default_path_cost")]
    pub path_cost: u32,
}

impl Default for BridgePort {
    fn default() -> Self {
        Self {
            pvid: None,
            vlans: BTreeMap::new(),
            isolated: false,
            edge: false,
            point_to_point: rstp_default(),
            path_cost: default_path_cost(),
        }
    }
}

fn default_path_cost() -> u32 {
    20_000
}
fn default_priority() -> u16 {
    32_768
}

fn rstp_default() -> bool {
    true
}

/// 桥内 VLAN 的本地三层接口，地址由正常 IP 接口 owner 应用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VlanGateway {
    /// 对应用与诊断工具可见的接口名。
    pub name: Name,
    #[serde(default)]
    #[schemars(schema_with = "super::optional_ipv4_schema")]
    pub address: Option<Ipv4Addr>,
    #[serde(default)]
    pub prefix_length: u8,
    #[serde(default, deserialize_with = "super::ipv6::addresses")]
    pub ipv6: Vec<super::Ipv6Address>,
    /// 仅向本 VLAN 发布的前缀、默认网关和 RDNSS。
    #[serde(default)]
    pub advertise: super::RouterAdvertisements,
    /// 可选的 DHCPv4 池，服务地址和子网来自本 VLAN 网关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dhcp_v4_server: Option<super::DhcpV4Pool>,
}

impl VlanGateway {
    fn dhcp_settings(&self) -> Result<Option<(Ipv4Addr, &super::DhcpV4Pool)>, Invalid> {
        let Some(options) = &self.dhcp_v4_server else {
            return Ok(None);
        };
        let address = self.address.ok_or(Invalid)?;
        Pool::validated_mask(address, self.prefix_length, options).ok_or(Invalid)?;
        Ok(Some((address, options)))
    }

    /// 将此 VLAN 的 DHCPv4 策略投影为标准服务池。
    /// # Errors
    /// 静态子网、池或选项非法，或无法复制 DNS 地址。
    pub fn dhcp_pool(&self) -> Result<Option<Pool>, Invalid> {
        self.dhcp_settings()?
            .map(|(address, options)| options.project(address, self.prefix_length))
            .transpose()
    }
}

/// 一个二层广播域的管理配置。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    /// 默认启用快速生成树。无环拓扑可显式关闭。
    #[serde(default = "rstp_default")]
    pub rstp: bool,
    /// 生成树选根优先级，按 4096 递增。
    #[serde(default = "default_priority")]
    pub priority: u16,
    /// 按稳定接口配置名引用物理端口。
    #[serde(deserialize_with = "unique_objects")]
    pub ports: BTreeMap<Name, BridgePort>,
    /// 以 VID 配置本机网关。缺席的 VLAN 保持纯二层交换。
    #[serde(
        default,
        deserialize_with = "unique_objects",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub gateways: BTreeMap<u16, VlanGateway>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            rstp: rstp_default(),
            priority: default_priority(),
            ports: BTreeMap::new(),
            gateways: BTreeMap::new(),
        }
    }
}

impl Interface {
    /// 检查端口加入网桥前需移除的三层配置。
    pub fn has_layer3_configuration(&self) -> bool {
        self.ipv4.is_some()
            || self.ipv6.is_some()
            || self.dhcp_v4_server.is_some()
            || self.default_route.is_some()
    }
}

impl Configuration {
    pub(super) fn vlan_gateway(&self, name: &Name) -> Option<&VlanGateway> {
        self.bridges
            .values()
            .flat_map(|bridge| bridge.gateways.values())
            .find(|gateway| gateway.name == *name)
    }

    /// 解析持久接口名称，返回物理匹配条件或 VLAN 的逻辑名称。
    pub fn interface_reference<'a>(&'a self, name: &'a Name) -> Option<InterfaceReference<'a>> {
        if let Some(profile) = self.interfaces.get(name) {
            return Some(InterfaceReference::Device(&profile.device));
        }
        self.vlan_gateway(name)
            .map(|_| InterfaceReference::Vlan(name))
    }

    pub(super) fn validate_bridges(&self) -> Result<(), ValidationError> {
        let mut assigned = BTreeSet::new();
        let mut gateway_names = BTreeSet::new();
        for (name, bridge) in &self.bridges {
            let path = format!(
                "/bridges/{}",
                name.as_str().replace('~', "~0").replace('/', "~1")
            );
            if bridge.priority & 0xfff != 0 {
                return Err(invalid(
                    format!("{path}/priority"),
                    "bridge priority must be a multiple of 4096",
                ));
            }
            if bridge.ports.is_empty() {
                return Err(invalid(
                    format!("{path}/ports"),
                    "bridge requires at least one port",
                ));
            }
            for (name, port) in &bridge.ports {
                let path = format!(
                    "{path}/ports/{}",
                    name.as_str().replace('~', "~0").replace('/', "~1")
                );
                let profile = self
                    .interfaces
                    .get(name)
                    .ok_or_else(|| invalid(path.clone(), "unknown interface profile"))?;
                if !assigned.insert(name) || assigned.len() > MAX_PORTS {
                    return Err(invalid(
                        path,
                        "interface already belongs to a bridge or port limit exceeded",
                    ));
                }
                if profile.has_layer3_configuration() {
                    return Err(invalid(
                        path,
                        "a physical bridge port cannot have its own IP configuration",
                    ));
                }
                if port.vlans.is_empty()
                    || port.vlans.keys().any(|&vid| !valid_vid(vid))
                    || port.pvid.is_some_and(|vid| !port.vlans.contains_key(&vid))
                {
                    return Err(invalid(path, "invalid VLAN membership or PVID"));
                }
                if !(1..=200_000_000).contains(&port.path_cost) {
                    return Err(invalid(
                        format!("{path}/path_cost"),
                        "invalid spanning tree path cost",
                    ));
                }
            }
            for (&vid, gateway) in &bridge.gateways {
                let path = format!("{path}/gateways/{vid}");
                if !valid_vid(vid)
                    || !bridge
                        .ports
                        .values()
                        .any(|port| port.vlans.contains_key(&vid))
                {
                    return Err(invalid(path, "gateway VLAN has no bridge member"));
                }
                if !crate::switching::valid_gateway_name(gateway.name.as_str().as_bytes())
                    || self.interfaces.contains_key(&gateway.name)
                    || !gateway_names.insert(&gateway.name)
                    || gateway_names.len() > MAX_PORTS
                {
                    return Err(invalid(path, "invalid or duplicate VLAN interface name"));
                }
                if !crate::switching::valid_gateway_address(gateway.address, gateway.prefix_length)
                    || crate::interface::Ipv6Addresses::new(&gateway.ipv6).is_err()
                    || gateway.ipv6.len() > usize::from(u16::MAX)
                {
                    return Err(invalid(path, "invalid VLAN interface addresses"));
                }
                if !gateway.advertise.valid_for(gateway.ipv6.iter().copied()) {
                    return Err(invalid(
                        format!("{path}/advertise"),
                        "invalid VLAN router advertisement policy",
                    ));
                }
                if gateway.dhcp_settings().is_err() {
                    return Err(invalid(
                        format!("{path}/dhcp_v4_server"),
                        "invalid VLAN DHCPv4 subnet or pool",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Locate the L2-only profile for a physical interface. Caller validates ambiguous device aliases separately.
    pub fn bridged_profile(&self, name: &[u8], mac: [u8; 6]) -> Option<&Interface> {
        self.bridges
            .values()
            .flat_map(|bridge| bridge.ports.keys())
            .filter_map(|port| self.interfaces.get(port))
            .find(|profile| profile.device.matches(name, mac))
    }

    /// Encode the full bridge membership for the currently present devices.
    /// Missing devices keep their persistent membership and are resolved again on arrival.
    /// # Errors
    /// Ambiguous device resolution, invalid configuration, or allocation failure.
    pub fn bridge_source(
        &self,
        mut resolve: impl FnMut(&super::DeviceMatch) -> Result<Option<u32>, Invalid>,
    ) -> Result<Vec<u8>, EncodeError> {
        self.validate_bridges().map_err(|_| EncodeError::Invalid)?;
        let count: usize = self.bridges.values().map(|bridge| bridge.ports.len()).sum();
        let mut ports = Vec::new();
        ports
            .try_reserve_exact(count)
            .map_err(|_| EncodeError::NoMemory)?;
        let mut number = 0u16;
        for (ordinal, bridge) in self.bridges.values().enumerate() {
            for (name, port) in &bridge.ports {
                number += 1;
                if let Some(index) =
                    resolve(&self.interfaces[name].device).map_err(|_| EncodeError::Invalid)?
                {
                    ports.push((index, (ordinal + 1) as u16, number, port));
                }
            }
        }
        ports.sort_unstable_by_key(|port| port.0);
        crate::switching::encode(self.bridges.values(), &ports)
    }
}
