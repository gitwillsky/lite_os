//! 持久上联 IA_PD 身份和下游 /64 分配意图。

use super::*;
use core::net::Ipv6Addr;

use crate::delegation::DelegatedPrefix;

/// 一个上联 IA_PD 为多个具名下游接口提供稳定的子网编号。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrefixDelegation {
    /// 上联对象名称，使用该对象的实际匹配设备。
    pub upstream: Name,
    /// DUID-LL 的固定以太网地址。写入后应在接口换名或更换硬件时保留。
    pub duid_mac: [u8; 6],
    /// 单一前缀委派身份，稳定保存，缺省为零。
    #[serde(default)]
    pub iaid: u32,
    /// 向可信上联服务器协商 RFC 9915 重配置密钥，缺省关闭。
    #[serde(default)]
    pub accept_reconfigure: bool,
    /// 从每个可容纳该编号的委派中分配 /64。编号在各下游之间必须唯一。
    #[serde(deserialize_with = "unique_objects")]
    pub downstream: BTreeMap<Name, u64>,
}

impl PrefixDelegation {
    /// 返回 RFC 9915 DUID-LL，以太网硬件类型为 1。
    pub fn duid(&self) -> [u8; 10] {
        let mut duid = [0, 3, 0, 1, 0, 0, 0, 0, 0, 0];
        duid[4..].copy_from_slice(&self.duid_mac);
        duid
    }

    /// 按下游编号推导租约覆盖的 /64，并保留上联授权的有效期。
    /// 重叠委派可分配同一 /64 时，更具体的授权优先。
    /// # Errors
    /// 无法为候选分配输出空间。
    pub fn subnets(
        &self,
        parents: &[DelegatedPrefix],
        subnet: u64,
        now: i64,
    ) -> Result<Vec<DelegatedPrefix>, alloc::collections::TryReserveError> {
        let mut result = Vec::new();
        result.try_reserve_exact(parents.len())?;
        for &parent in parents {
            let Some(prefix) = delegated_subnet(parent, subnet, now) else {
                continue;
            };
            let shadowed = parents.iter().any(|&other| {
                if other.length <= parent.length || other.length > 64 {
                    return false;
                }
                let host = ((u128::from(prefix.address) ^ u128::from(other.address)) >> 64) as u64;
                self.downstream.values().any(|&id| id == host)
                    && delegated_subnet(other, host, now)
                        .is_some_and(|child| child.address == prefix.address)
            });
            if !shadowed {
                result.push(prefix);
            }
        }
        result.sort_unstable_by_key(|prefix| prefix.address);
        Ok(result)
    }

    pub(super) fn validate(&self, configuration: &Configuration) -> Result<(), ValidationError> {
        if self.duid_mac == [0; 6] || self.duid_mac[0] & 1 != 0 {
            return Err(invalid(
                "/prefix_delegation/duid_mac",
                "expected a unicast Ethernet address",
            ));
        }
        if !configuration.interfaces.contains_key(&self.upstream)
            || configuration
                .bridges
                .values()
                .any(|bridge| bridge.ports.contains_key(&self.upstream))
        {
            return Err(invalid(
                "/prefix_delegation/upstream",
                "expected a routed physical interface",
            ));
        }
        if self.downstream.is_empty() {
            return Err(invalid(
                "/prefix_delegation/downstream",
                "expected a downstream interface",
            ));
        }
        let mut identifiers = BTreeSet::new();
        for (name, identifier) in &self.downstream {
            let advertised = configuration
                .interfaces
                .get(name)
                .and_then(|interface| interface.ipv6.as_ref())
                .is_some_and(|ipv6| ipv6.advertise != RouterAdvertisements::Disabled)
                || configuration
                    .vlan_gateway(name)
                    .is_some_and(|gateway| gateway.advertise != RouterAdvertisements::Disabled);
            if name == &self.upstream || !advertised || !identifiers.insert(identifier) {
                return Err(invalid(
                    "/prefix_delegation/downstream",
                    "requires distinct downstream interfaces with advertisements and unique subnet identifiers",
                ));
            }
        }
        Ok(())
    }
}

// 租约解码已校验上联前缀的规范位。
fn delegated_subnet(parent: DelegatedPrefix, subnet: u64, now: i64) -> Option<DelegatedPrefix> {
    let bits = 64u8.checked_sub(parent.length)?;
    if parent.valid_until <= now || bits < 64 && subnet >> bits != 0 {
        return None;
    }
    let prefix = Ipv6Addr::from(u128::from(parent.address) | (u128::from(subnet) << 64));
    if prefix.is_unspecified() || prefix.is_multicast() || prefix.is_unicast_link_local() {
        return None;
    }
    Some(DelegatedPrefix {
        address: prefix,
        length: 64,
        ..parent
    })
}
