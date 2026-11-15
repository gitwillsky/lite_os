//! 接口地址配置和 Linux 链路事实。可编辑条目只来自当前路由域已保存的静态地址。

use super::Family;
use crate::configuration::DraftBaseline;
use linux_uapi::rtnetlink::{LinkAddress, LinkStatistics};
use network_config::{
    ManagementClient,
    configuration::{
        Assignment, ConfigurationName as Name, InterfaceState, Ipv6Address, RouterConfiguration,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{IpAddr, Ipv4Addr},
};

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Interface {
    Configured(Name),
    Hardware { name: Vec<u8>, mac: [u8; 6] },
    Gateway { bridge: Name, vid: u16 },
}

#[derive(PartialEq, Eq)]
pub(super) struct Target {
    pub(super) interface: Interface,
    pub(super) name: String,
    pub(super) available: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Member {
    pub(super) interface: Interface,
    pub(super) address: Address,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Address {
    Ipv4(Ipv4Addr, u8),
    Ipv6(Ipv6Address),
}

impl Address {
    pub(super) fn parts(self) -> (IpAddr, u8) {
        match self {
            Self::Ipv4(address, prefix) => (address.into(), prefix),
            Self::Ipv6(address) => (address.address.into(), address.prefix_length),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) member: Option<Member>,
    pub(super) runtime: Option<LinkAddress>,
    pub(super) address: IpAddr,
    dynamic: bool,
    pub(super) prefix: u8,
    pub(super) interface: String,
}

impl Entry {
    pub(super) fn flags(&self) -> String {
        let invalid = self.runtime.is_none_or(LinkAddress::unavailable);
        let scope = match self.address {
            IpAddr::V6(address) if address.is_unicast_link_local() => "L",
            IpAddr::V6(address) if !address.is_loopback() => "G",
            _ => "",
        };
        format!(
            "{}{}{scope}",
            if invalid { "I" } else { "" },
            if self.dynamic { "D" } else { "" }
        )
    }

    pub(super) fn address_text(&self) -> String {
        format!("{}/{}", self.address, self.prefix)
    }

    pub(super) fn network(&self) -> Option<Ipv4Addr> {
        let IpAddr::V4(address) = self.address else {
            return None;
        };
        Some(network(address, self.prefix))
    }

    /// 与寿命倒数和 DAD 状态无关的列表选择身份。
    pub(super) fn same_address(&self, next: &Self) -> bool {
        if let Some(member) = &self.member {
            return next.member.as_ref() == Some(member);
        }
        next.member.is_none()
            && self.address == next.address
            && self.prefix == next.prefix
            && self.runtime.map(|row| row.index) == next.runtime.map(|row| row.index)
            && self.interface == next.interface
    }
}

pub(super) fn network(address: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    Ipv4Addr::from(u32::from(address) & mask)
}

pub(super) struct Catalog {
    pub(super) baseline: Option<DraftBaseline>,
    pub(super) targets: Vec<Target>,
    pub(super) entries: Vec<Entry>,
}

/// 对照已保存地址和同一路由域的真实链路地址。
///
/// 错误：路由域消失、scope 无法访问、配置或 Linux 地址读取失败，或期间配置发生变化。
pub(super) fn load(scope: Option<&str>, family: Family) -> io::Result<Catalog> {
    let mut client = ManagementClient::local();
    let mut dhcp = BTreeSet::new();
    let (revision, document) = match client.export() {
        Ok(baseline) => baseline,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // 运行地址可经标准 Linux UAPI 读取；配置导出权限独立决定静态编辑是否可用。
            let (links, live) = runtime(scope, family)?;
            if family == Family::Ipv4 && scope.is_none() {
                let snapshot = ManagementClient::local().snapshot()?;
                record_leases(&snapshot.interfaces, &links, &mut dhcp);
            }
            let mut catalog = Catalog {
                baseline: None,
                targets: Vec::new(),
                entries: Vec::new(),
            };
            catalog.add_runtime(live, &links, &dhcp)?;
            return Ok(catalog);
        }
        Err(error) => return Err(error),
    };
    let config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
    let (interfaces, bridges) = if let Some(scope) = scope {
        let domain = config
            .routing_domains
            .get(scope)
            .ok_or_else(scope_missing)?;
        (&domain.interfaces, &domain.bridges)
    } else {
        (&config.interfaces, &config.bridges)
    };
    let (links, mut live) = runtime(scope, family)?;
    let snapshot = client.snapshot()?;
    if snapshot.generation != revision {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if family == Family::Ipv4 && scope.is_none() {
        record_leases(&snapshot.interfaces, &links, &mut dhcp);
    }
    let mut catalog = Catalog {
        baseline: Some(DraftBaseline::new(revision, document.into())),
        targets: Vec::new(),
        entries: Vec::new(),
    };
    let gateway_names: BTreeSet<_> = bridges
        .values()
        .flat_map(|bridge| bridge.gateways.values())
        .map(|gateway| gateway.name.as_str().as_bytes())
        .collect();
    let mut matched = BTreeSet::new();
    for (name, profile) in interfaces {
        // VLAN 网关可继承下层 MAC，按设备 MAC 关联时排除已辨明的虚拟网关。
        let mut matches = links.values().filter(|link| {
            !gateway_names.contains(link_name(link))
                && link
                    .ethernet_address
                    .is_some_and(|mac| profile.device.matches(link_name(link), mac))
        });
        let link = matches.next();
        let link = link.filter(|_| matches.next().is_none());
        if let Some(link) = link {
            matched.insert(link.index);
        }
        if bridges
            .values()
            .any(|bridge| bridge.ports.contains_key(name))
        {
            continue;
        }
        let target = |available| Target {
            interface: Interface::Configured(name.clone()),
            name: link.map_or_else(|| name.as_str().to_owned(), display_name),
            available,
        };
        match family {
            Family::Ipv4 => {
                if scope.is_some()
                    && let Some(link) = link
                    && profile
                        .ipv4
                        .as_ref()
                        .is_some_and(|ipv4| ipv4.assignment == Assignment::Dhcp)
                {
                    for row in ipv4_on(&live, link.index) {
                        dhcp.insert((link.index, row.address));
                    }
                }
                let configured = profile
                    .ipv4
                    .as_ref()
                    .filter(|ipv4| {
                        ipv4.assignment == Assignment::Static && !ipv4.address.is_unspecified()
                    })
                    .map(|ipv4| Address::Ipv4(ipv4.address, ipv4.prefix_length));
                // 默认域没有独立的空 IPv4 槽；None 继承系统策略。只在具名域的空配置新建。
                let vacant = scope.is_some() && profile.ipv4.is_none() && vacant_link(&live, link);
                catalog.add_target(target(vacant), link, configured, &mut live);
            }
            Family::Ipv6 => catalog.add_target(
                target(true),
                link,
                profile
                    .ipv6
                    .iter()
                    .flat_map(|profile| profile.addresses.iter().copied().map(Address::Ipv6)),
                &mut live,
            ),
        }
    }
    for (bridge, config) in bridges {
        for (vid, gateway) in &config.gateways {
            let link = links
                .values()
                .find(|link| link_name(link) == gateway.name.as_str().as_bytes());
            if let Some(link) = link {
                matched.insert(link.index);
            }
            let target = Target {
                interface: Interface::Gateway {
                    bridge: bridge.clone(),
                    vid: *vid,
                },
                name: gateway.name.as_str().to_owned(),
                available: family == Family::Ipv6
                    || gateway.address.is_none() && vacant_link(&live, link),
            };
            match family {
                Family::Ipv4 => catalog.add_target(
                    target,
                    link,
                    gateway
                        .address
                        .map(|address| Address::Ipv4(address, gateway.prefix_length)),
                    &mut live,
                ),
                Family::Ipv6 => catalog.add_target(
                    target,
                    link,
                    gateway.ipv6.iter().copied().map(Address::Ipv6),
                    &mut live,
                ),
            }
        }
    }
    if scope.is_none() && family == Family::Ipv6 {
        for link in links.values().filter(|link| !matched.contains(&link.index)) {
            if let Some(mac) = link.ethernet_address {
                catalog.targets.push(Target {
                    interface: Interface::Hardware {
                        name: link_name(link).to_vec(),
                        mac,
                    },
                    name: display_name(link),
                    available: true,
                });
            }
        }
    }
    catalog.targets.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.add_runtime(live, &links, &dhcp)?;
    Ok(catalog)
}

impl Catalog {
    fn add_runtime(
        &mut self,
        live: LiveAddresses,
        links: &Links,
        dhcp: &DhcpAddresses,
    ) -> io::Result<()> {
        for ((index, address, prefix), runtime) in live {
            let link = links.get(&index).ok_or(io::ErrorKind::Interrupted)?;
            self.entries.push(Entry {
                member: None,
                runtime: Some(runtime),
                address,
                dynamic: dhcp.contains(&(index, address))
                    || address.is_loopback()
                    || matches!(address, IpAddr::V6(address) if address.is_unicast_link_local())
                    || !runtime.permanent(),
                prefix,
                interface: display_name(link),
            });
        }
        self.entries.sort_by(|a, b| {
            (&a.interface, a.address, a.prefix).cmp(&(&b.interface, b.address, b.prefix))
        });
        Ok(())
    }

    fn add_target(
        &mut self,
        target: Target,
        link: Option<&LinkStatistics>,
        configured: impl IntoIterator<Item = Address>,
        live: &mut LiveAddresses,
    ) {
        for address in configured {
            let (ip, prefix) = address.parts();
            let runtime = link.and_then(|link| live.remove(&(link.index, ip, prefix)));
            self.entries.push(Entry {
                member: Some(Member {
                    interface: target.interface.clone(),
                    address,
                }),
                runtime,
                address: ip,
                dynamic: false,
                prefix,
                interface: target.name.clone(),
            });
        }
        self.targets.push(target);
    }
}

type Links = BTreeMap<u32, LinkStatistics>;
type LiveAddresses = BTreeMap<(u32, IpAddr, u8), LinkAddress>;
type DhcpAddresses = BTreeSet<(u32, IpAddr)>;

fn record_leases(interfaces: &[InterfaceState], links: &Links, leases: &mut DhcpAddresses) {
    for device in interfaces.iter().filter(|device| device.lease.is_some()) {
        if let Some(link) = links.values().find(|link| {
            link_name(link) == device.name && link.ethernet_address == Some(device.mac)
        }) {
            leases.insert((link.index, device.address.into()));
        }
    }
}

fn vacant_link(live: &LiveAddresses, link: Option<&LinkStatistics>) -> bool {
    link.is_none_or(|link| ipv4_on(live, link.index).next().is_none())
}

fn ipv4_on(live: &LiveAddresses, index: u32) -> impl Iterator<Item = &LinkAddress> {
    live.range((index, Ipv4Addr::UNSPECIFIED.into(), 0)..=(index, Ipv4Addr::BROADCAST.into(), 32))
        .map(|(_, row)| row)
}

fn runtime(scope: Option<&str>, family: Family) -> io::Result<(Links, LiveAddresses)> {
    crate::network_scope::read(scope, move || read_netlink(family))
}

#[cfg(target_os = "linux")]
fn read_netlink(family: Family) -> io::Result<(Links, LiveAddresses)> {
    use linux_uapi::rtnetlink::LinkReader;
    use std::time::Duration;

    const DUMP_TIMEOUT: Duration = Duration::from_secs(2);
    let mut reader = LinkReader::open()?;
    let mut links = BTreeMap::new();
    reader.read(DUMP_TIMEOUT, |link| {
        links.insert(link.index, link);
    })?;
    let mut addresses = BTreeMap::new();
    reader.read_addresses(DUMP_TIMEOUT, |row| {
        if super::is_family(family, row.address) {
            addresses.insert((row.index, row.address, row.prefix_length), row);
        }
    })?;
    Ok((links, addresses))
}

#[cfg(not(target_os = "linux"))]
fn read_netlink(_family: Family) -> io::Result<(Links, LiveAddresses)> {
    Err(io::ErrorKind::Unsupported.into())
}

fn link_name(link: &LinkStatistics) -> &[u8] {
    let end = link
        .name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(link.name.len());
    &link.name[..end]
}

fn display_name(link: &LinkStatistics) -> String {
    String::from_utf8_lossy(link_name(link)).into_owned()
}

pub(super) enum Change {
    Put {
        previous: Option<Member>,
        next: Member,
    },
    Remove(Member),
}

pub(super) fn scope_missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域已不存在。")
}
