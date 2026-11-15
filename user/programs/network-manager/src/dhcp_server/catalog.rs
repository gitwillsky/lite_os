//! 服务器、网段和租约分别投影已保存配置与执行服务事实。

use crate::configuration::{
    DraftBaseline,
    devices::{Device, DeviceMut, Layer3},
};
use network_config::{
    ManagementClient, Revision,
    configuration::{Assignment, ConfigurationName as Name, DhcpV4Pool, RouterConfiguration},
    format_mac,
};
use std::{io, net::Ipv4Addr, time::Duration};

pub(super) struct Catalog {
    pub(super) baseline: DraftBaseline,
    pub(super) targets: Vec<Server>,
}

#[derive(Clone)]
pub(super) struct Server {
    pub(super) interface: Name,
    pub(super) address: Ipv4Addr,
    pub(super) prefix: u8,
    pub(super) pool: Option<DhcpV4Pool>,
}

impl Server {
    pub(super) fn network(&self) -> String {
        let mask = u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0);
        format!(
            "{}/{}",
            Ipv4Addr::from(u32::from(self.address) & mask),
            self.prefix
        )
    }
}

impl Catalog {
    /// 读取当前域的实际 DHCP 配置和可承载分发的静态接口。
    /// 错误：管理端点不可用、域消失或配置编码无效。
    pub(super) fn load(
        domain: Option<&str>,
        current: Option<Revision>,
    ) -> io::Result<Option<Self>> {
        let Some((baseline, config)) = DraftBaseline::capture(current)? else {
            return Ok(None);
        };
        let scope = Layer3::new(&config, domain)?;
        let mut targets: Vec<_> = scope
            .devices()
            .filter_map(|device| match device {
                Device::Interface(name, profile) => {
                    let address = profile.ipv4.as_ref().filter(|ip| {
                        ip.assignment == Assignment::Static
                            && ip.prefix_length <= 30
                            && profile.mtu >= 576
                    })?;
                    Some(Server {
                        interface: name.clone(),
                        address: address.address,
                        prefix: address.prefix_length,
                        pool: profile.dhcp_v4_server.clone(),
                    })
                }
                Device::Gateway(gateway) => {
                    let address = gateway.address.filter(|_| gateway.prefix_length <= 30)?;
                    Some(Server {
                        interface: gateway.name.clone(),
                        address,
                        prefix: gateway.prefix_length,
                        pool: gateway.dhcp_v4_server.clone(),
                    })
                }
            })
            .collect();
        targets.sort_unstable_by(|a, b| a.interface.cmp(&b.interface));
        Ok(Some(Self { baseline, targets }))
    }
}

/// 设置或清除指定下游的 DHCP 池。错误：目标已移动或不再承载三层设置。
pub(super) fn set_pool(
    config: &mut RouterConfiguration,
    domain: Option<&str>,
    interface: &Name,
    next: Option<DhcpV4Pool>,
) -> io::Result<()> {
    let slot = match DeviceMut::find(config, domain, interface)? {
        DeviceMut::Interface(port) => &mut port.dhcp_v4_server,
        DeviceMut::Gateway(gateway) => &mut gateway.dhcp_v4_server,
    };
    *slot = next;
    Ok(())
}

#[derive(Clone)]
pub(super) struct Lease {
    pub(super) interface: String,
    pub(super) address: Ipv4Addr,
    pub(super) mac: String,
    pub(super) remaining: Option<Duration>,
}

/// 读取同一路由域中的活动租约。观察权限独立于配置导出权限。
/// 错误：链路或租约查询失败、池在分页期间变化，或回复格式无效。
pub(super) fn leases(domain: Option<&str>) -> io::Result<Vec<Lease>> {
    let links = crate::network_scope::read(domain, ethernet_names)?;
    let mut client = ManagementClient::local();
    let mut leases = Vec::new();
    let mut revision = None;
    for name in links {
        let interface = String::from_utf8_lossy(&name);
        let mut after = Ipv4Addr::UNSPECIFIED;
        loop {
            let (current, observed, rows, more) = match client
                .dhcp_server_bindings(domain, &name, after)
            {
                Ok(page) => page,
                Err(error) if after.is_unspecified() && error.kind() == io::ErrorKind::NotFound => {
                    break;
                }
                Err(error) => return Err(error),
            };
            if revision.is_some_and(|previous| previous != current) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            revision = Some(current);
            for row in rows {
                after = row.address;
                leases.push(Lease {
                    interface: interface.to_string(),
                    address: row.address,
                    mac: format_mac(row.hardware_address),
                    remaining: (row.expires_ms != u64::MAX)
                        .then(|| Duration::from_millis(row.expires_ms.saturating_sub(observed))),
                });
            }
            if !more {
                break;
            }
        }
    }
    Ok(leases)
}

#[cfg(target_os = "linux")]
fn ethernet_names() -> io::Result<Vec<Vec<u8>>> {
    let mut reader = linux_uapi::rtnetlink::LinkReader::open()?;
    let mut links = Vec::new();
    reader.read(Duration::from_secs(2), |link| {
        if link.ethernet_address.is_some() && link.index != 1 {
            let end = link
                .name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(link.name.len());
            links.push(link.name[..end].to_vec());
        }
    })?;
    links.sort_unstable();
    Ok(links)
}

#[cfg(not(target_os = "linux"))]
fn ethernet_names() -> io::Result<Vec<Vec<u8>>> {
    Err(io::ErrorKind::Unsupported.into())
}
