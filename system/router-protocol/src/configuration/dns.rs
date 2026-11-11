//! 持久 DNS 转发意图，接口始终使用配置对象名称。

use super::*;
use core::net::IpAddr;

/// 明确在一个已配置地址上提供下游 DNS。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsListener {
    pub interface: Name,
    #[schemars(with = "String")]
    pub address: IpAddr,
}

/// 上游地址。链路本地 IPv6 地址必须指明接口。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsUpstream {
    #[schemars(with = "String")]
    pub address: IpAddr,
    #[serde(default)]
    pub interface: Option<Name>,
}

/// 完整 DNS 转发设置，关闭时不删除接口上的其他解析设置。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsForwarding {
    pub listen: Vec<DnsListener>,
    pub upstream: Vec<DnsUpstream>,
}

impl DnsForwarding {
    pub(super) fn validate(&self, configuration: &Configuration) -> Result<(), ValidationError> {
        let path = "/dns/forwarding";
        if !(1..=32).contains(&self.listen.len()) || !(1..=8).contains(&self.upstream.len()) {
            return Err(invalid(
                path,
                "expected 1..32 listeners and 1..8 upstream resolvers",
            ));
        }
        for (index, listener) in self.listen.iter().enumerate() {
            let profile = configuration
                .interfaces
                .get(&listener.interface)
                .ok_or_else(|| invalid(path, "listener interface does not exist"))?;
            let configured = match listener.address {
                IpAddr::V4(address) => profile.ipv4.as_ref().is_some_and(|ipv4| {
                    ipv4.assignment == Assignment::Static && ipv4.address == address
                }),
                IpAddr::V6(address) => profile.ipv6.as_ref().is_some_and(|ipv6| {
                    ipv6.addresses.iter().any(|entry| entry.address == address)
                }),
            };
            if !configured || self.listen[..index].contains(listener) {
                return Err(invalid(
                    path,
                    "listener must use a unique configured static address",
                ));
            }
        }
        for (index, upstream) in self.upstream.iter().enumerate() {
            let scoped =
                matches!(upstream.address, IpAddr::V6(address) if address.is_unicast_link_local());
            let valid = match upstream.address {
                IpAddr::V4(address) => {
                    !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast()
                }
                IpAddr::V6(address) => {
                    !address.is_unspecified()
                        && !address.is_multicast()
                        && address.to_ipv4_mapped().is_none()
                }
            };
            if !valid
                || scoped != upstream.interface.is_some()
                || upstream
                    .interface
                    .as_ref()
                    .is_some_and(|name| !configuration.interfaces.contains_key(name))
                || self.upstream[..index].contains(upstream)
                || self.listen.iter().any(|listener| {
                    listener.address == upstream.address
                        && (!scoped || upstream.interface.as_ref() == Some(&listener.interface))
                })
            {
                return Err(invalid(
                    path,
                    "invalid, duplicate or self-referential upstream resolver",
                ));
            }
        }
        Ok(())
    }
}
