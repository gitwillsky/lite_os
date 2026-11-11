//! Per-interface DHCPv4 settings. The server address and subnet come from static IPv4 configuration.

use super::*;
use crate::dhcp_server::Pool;

/// 地址池、租期及对客户端发布的网关和 DNS。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DhcpV4Pool {
    #[schemars(schema_with = "ipv4_schema")]
    pub first: Ipv4Addr,
    #[schemars(schema_with = "ipv4_schema")]
    pub last: Ipv4Addr,
    #[serde(default)]
    #[schemars(with = "Option<String>")]
    pub router: Option<Ipv4Addr>,
    #[serde(default)]
    #[schemars(schema_with = "ipv4_list_schema")]
    pub dns: Vec<Ipv4Addr>,
    #[schemars(range(min = 1))]
    pub lease_seconds: u32,
    /// 新分配前默认检查直连地址是否已占用，管理员可按接口关闭。
    #[serde(default = "probe_addresses_default")]
    pub probe_addresses: bool,
}

fn probe_addresses_default() -> bool {
    true
}

impl Interface {
    /// Derive a server pool from the same static address used by the interface.
    /// # Errors
    /// A pool requires a static IPv4 subnet and valid host addresses/options.
    pub fn dhcp_pool(&self) -> Result<Option<Pool>, Invalid> {
        let Some((ipv4, options)) = self.dhcp_settings()? else {
            return Ok(None);
        };
        let mut dns = Vec::new();
        dns.try_reserve_exact(options.dns.len())
            .map_err(|_| Invalid)?;
        dns.extend_from_slice(&options.dns);
        Ok(Some(Pool {
            server: ipv4.address,
            prefix_length: ipv4.prefix_length,
            options: DhcpV4Pool {
                first: options.first,
                last: options.last,
                router: options.router,
                dns,
                lease_seconds: options.lease_seconds,
                probe_addresses: options.probe_addresses,
            },
        }))
    }

    pub(super) fn dhcp_settings(
        &self,
    ) -> Result<Option<(&Ipv4Configuration, &DhcpV4Pool)>, Invalid> {
        let Some(options) = &self.dhcp_v4_server else {
            return Ok(None);
        };
        let ipv4 = self
            .ipv4
            .as_ref()
            .filter(|ipv4| ipv4.assignment == Assignment::Static)
            .ok_or(Invalid)?;
        if self.mtu < 576
            || Pool::validated_mask(ipv4.address, ipv4.prefix_length, options).is_none()
        {
            return Err(Invalid);
        }
        Ok(Some((ipv4, options)))
    }
}
