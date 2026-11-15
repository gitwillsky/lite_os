//! 地址编辑候选。按原生配置约束修改网卡或 VLAN 网关，同一事务完成移动。

use super::catalog::{Address, Change, Interface, Member, scope_missing};
use crate::configuration::DraftBaseline;
use network_config::{
    ManagementClient, Revision,
    configuration::{
        Assignment, Ipv4Configuration, Ipv6Address, Ipv6Configuration, RouterConfiguration,
        RouterInterface, VlanGateway,
    },
};
use std::{io, net::Ipv4Addr, sync::Arc};

/// 在已验证的配置快照上生成文档，提交时仍由配置服务匹配代际。
///
/// 错误：配置、选中条目或目标已变化，或候选不满足原生配置约束。
pub(super) fn candidate(
    scope: Option<&str>,
    baseline: DraftBaseline,
    change: Change,
) -> io::Result<(Revision, Arc<[u8]>)> {
    let (actual, document) = ManagementClient::local().export()?;
    baseline.verify(actual, &document)?;
    let mut config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
    match change {
        Change::Put {
            previous: Some(previous),
            next,
        } if previous.interface == next.interface => {
            owner(&mut config, scope, &next.interface)?.edit(
                Some(previous.address),
                Some(next.address),
                scope.is_some(),
            )?;
        }
        Change::Put { previous, next } => {
            if let Some(previous) = previous {
                remove(&mut config, scope, previous)?;
            }
            owner(&mut config, scope, &next.interface)?.edit(
                None,
                Some(next.address),
                scope.is_some(),
            )?;
        }
        Change::Remove(previous) => remove(&mut config, scope, previous)?,
    }
    let next = config.encode().map_err(io::Error::other)?;
    Ok((actual, next.into()))
}

fn remove(config: &mut RouterConfiguration, scope: Option<&str>, member: Member) -> io::Result<()> {
    owner(config, scope, &member.interface)?.edit(Some(member.address), None, scope.is_some())
}

enum Owner<'a> {
    Profile(&'a mut RouterInterface),
    Gateway(&'a mut VlanGateway),
}

impl Owner<'_> {
    fn edit(self, previous: Option<Address>, next: Option<Address>, named: bool) -> io::Result<()> {
        match (previous, next) {
            (Some(Address::Ipv4(..)), None)
            | (None, Some(Address::Ipv4(..)))
            | (Some(Address::Ipv4(..)), Some(Address::Ipv4(..))) => {
                self.ipv4(previous, next, named)
            }
            (Some(Address::Ipv6(..)), None)
            | (None, Some(Address::Ipv6(..)))
            | (Some(Address::Ipv6(..)), Some(Address::Ipv6(..))) => self.ipv6(previous, next),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    fn ipv4(self, previous: Option<Address>, next: Option<Address>, named: bool) -> io::Result<()> {
        match self {
            Self::Profile(profile) => edit_ipv4_profile(&mut profile.ipv4, previous, next, named),
            Self::Gateway(gateway) => {
                let current = gateway
                    .address
                    .map(|address| Address::Ipv4(address, gateway.prefix_length));
                if current != previous {
                    return Err(occupied());
                }
                (gateway.address, gateway.prefix_length) = match next {
                    Some(Address::Ipv4(address, prefix)) => (Some(address), prefix),
                    None => (None, 0),
                    _ => unreachable!("IPv4 family established by edit"),
                };
                Ok(())
            }
        }
    }

    fn ipv6(self, previous: Option<Address>, next: Option<Address>) -> io::Result<()> {
        let addresses = match self {
            Self::Profile(profile) => {
                &mut profile
                    .ipv6
                    .get_or_insert_with(|| Ipv6Configuration {
                        addresses: Vec::new(),
                        accept_ra: Default::default(),
                        advertise: Default::default(),
                    })
                    .addresses
            }
            Self::Gateway(gateway) => &mut gateway.ipv6,
        };
        if let Some(Address::Ipv6(previous)) = previous {
            let position = addresses
                .iter()
                .position(|address| *address == previous)
                .ok_or_else(|| io::Error::other("所选静态地址已经不存在。"))?;
            addresses.remove(position);
        }
        if let Some(Address::Ipv6(next)) = next {
            let position = addresses
                .binary_search_by_key(&next.address, |address: &Ipv6Address| address.address)
                .err()
                .ok_or_else(|| io::Error::other("该接口已有相同的静态地址。"))?;
            addresses.insert(position, next);
        }
        Ok(())
    }
}

fn edit_ipv4_profile(
    profile: &mut Option<Ipv4Configuration>,
    previous: Option<Address>,
    next: Option<Address>,
    named: bool,
) -> io::Result<()> {
    match (previous, profile.as_ref()) {
        (Some(Address::Ipv4(address, prefix)), Some(profile))
            if profile.assignment == Assignment::Static
                && profile.address == address
                && profile.prefix_length == prefix => {}
        // 默认域的 None 意味着继承系统策略；不能借新建地址接管它。
        (None, None) if named => {}
        _ => return Err(occupied()),
    }
    if let Some(Address::Ipv4(address, prefix_length)) = next {
        let configured = profile.get_or_insert_with(|| Ipv4Configuration {
            assignment: Assignment::Static,
            address,
            prefix_length,
            gateway: Ipv4Addr::UNSPECIFIED,
            dns: Vec::new(),
            dns_suffix: String::new(),
        });
        configured.address = address;
        configured.prefix_length = prefix_length;
    } else {
        *profile = None;
    }
    Ok(())
}

fn owner<'a>(
    config: &'a mut RouterConfiguration,
    scope: Option<&str>,
    target: &Interface,
) -> io::Result<Owner<'a>> {
    if let Interface::Hardware { name, mac } = target {
        if scope.is_some() || config.routing_domain_for_mac(*mac).is_some() {
            return Err(interface_missing());
        }
        return config
            .ensure_interface(name, Some(*mac))
            .map(Owner::Profile)
            .map_err(|_| interface_missing());
    }
    let (interfaces, bridges) = if let Some(scope) = scope {
        let domain = config
            .routing_domains
            .get_mut(scope)
            .ok_or_else(scope_missing)?;
        (&mut domain.interfaces, &mut domain.bridges)
    } else {
        (&mut config.interfaces, &mut config.bridges)
    };
    match target {
        Interface::Configured(name) => {
            if bridges
                .values()
                .any(|bridge| bridge.ports.contains_key(name))
            {
                return Err(interface_missing());
            }
            interfaces
                .get_mut(name)
                .map(Owner::Profile)
                .ok_or_else(interface_missing)
        }
        Interface::Gateway { bridge, vid } => bridges
            .get_mut(bridge)
            .and_then(|bridge| bridge.gateways.get_mut(vid))
            .map(Owner::Gateway)
            .ok_or_else(interface_missing),
        Interface::Hardware { .. } => unreachable!("handled before selecting profile"),
    }
}

fn occupied() -> io::Error {
    io::Error::other("所选接口的 IPv4 地址或获取方式已变更。请选择空闲接口或重新打开地址属性。")
}

fn interface_missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选接口已不存在或不可配置地址。")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_address_does_not_replace_dhcp_and_an_edit_keeps_interface_policy() {
        let first = Ipv4Addr::new(192, 0, 2, 11);
        let second = Ipv4Addr::new(192, 0, 2, 12);
        let next = Some(Address::Ipv4(second, 24));
        let gateway = Ipv4Addr::new(192, 0, 2, 1);

        let mut inherited = None;
        assert!(edit_ipv4_profile(&mut inherited, None, next, false).is_err());
        assert!(inherited.is_none());

        let mut configured = Some(Ipv4Configuration {
            assignment: Assignment::Dhcp,
            address: first,
            prefix_length: 24,
            gateway,
            dns: vec![gateway],
            dns_suffix: "example.test".into(),
        });
        let before = configured.clone();
        assert!(edit_ipv4_profile(&mut configured, None, next, true).is_err());
        assert_eq!(configured, before);

        configured.as_mut().unwrap().assignment = Assignment::Static;
        edit_ipv4_profile(&mut configured, Some(Address::Ipv4(first, 24)), next, true).unwrap();
        let profile = configured.unwrap();
        assert_eq!(
            (profile.address, profile.gateway, profile.dns),
            (second, gateway, vec![gateway])
        );
        assert_eq!(profile.dns_suffix, "example.test");
    }
}
