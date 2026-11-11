//! 候选字段、设备、源与直接下一跳的准入；不能借将被替换的受管路由证明网关直达。
use super::*;
use network_protocol::routes::{NextHopMember, RouteConfiguration, RouteConfigurationBatch};
fn available(interface: &NetworkInterface, changes: &[InterfaceChange]) -> bool {
    let change = changes
        .iter()
        .find(|change| change.index == interface.index);
    change.map_or(interface.state.up, |change| change.next.up)
        && change.map_or_else(|| interface.device.ip_mtu(), InterfaceChange::next_mtu) >= 1280
        && interface.ipv6.is_some()
}
pub(in crate::inet::route) fn member<'a>(
    path: NextHopMember<Ipv6Addr>,
    interfaces: impl Clone + Iterator<Item = &'a NetworkInterface>,
    changes: &[InterfaceChange],
) -> Result<(), SocketError> {
    let interface = interfaces
        .clone()
        .find(|interface| interface.index == path.interface_index)
        .ok_or(SocketError::NoDevice)?;
    if !available(interface, changes) {
        return Err(SocketError::NetworkUnreachable);
    }
    let Some(gateway) = path.gateway else {
        return if path.on_link {
            Err(SocketError::Invalid)
        } else {
            Ok(())
        };
    };
    if gateway.is_unspecified()
        || gateway.is_multicast()
        || gateway.is_loopback()
        || gateway.to_ipv4_mapped().is_some()
        || interface.index == 1
        || interfaces.clone().any(|interface| {
            definitions(interface, changes).any(|address| address.address == gateway)
        })
    {
        return Err(SocketError::Invalid);
    }
    if path.on_link
        || gateway.is_unicast_link_local()
        || definitions(interface, changes).any(|address| {
            address.on_link
                && (u128::from(address.address) ^ u128::from(gateway)).leading_zeros()
                    >= u32::from(address.prefix_length)
        })
    {
        Ok(())
    } else {
        Err(SocketError::NetworkUnreachable)
    }
}
fn attributes<'a>(
    mut route: RouteAttributes<Ipv6Addr>,
    interfaces: impl Clone + Iterator<Item = &'a NetworkInterface>,
    changes: &[InterfaceChange],
) -> Result<RouteAttributes<Ipv6Addr>, SocketError> {
    if route.prefix_length > 128
        || u128::from(route.destination).masked(route.prefix_length)
            != u128::from(route.destination)
        || route.table != TABLE_MAIN
            && !router_protocol::policy_routing::alternate_table(route.table)
        || route.scope > 253
        || !matches!(
            route.kind,
            RouteKind::Unicast
                | RouteKind::Blackhole
                | RouteKind::Unreachable
                | RouteKind::Prohibit
        )
    {
        return Err(SocketError::Invalid);
    }
    route.preferred_source = route
        .preferred_source
        .filter(|source| !source.is_unspecified());
    if let Some(source) = route.preferred_source
        && !interfaces.clone().any(|interface| {
            available(interface, changes)
                && definitions(interface, changes).any(|address| address.address == source)
        })
    {
        return Err(SocketError::AddressNotAvailable);
    }
    Ok(route)
}
fn source_scope(
    interface: &NetworkInterface,
    source: Option<Ipv6Addr>,
    changes: &[InterfaceChange],
) -> bool {
    source.is_none_or(|source| {
        (!source.is_loopback() || interface.index == 1)
            && (!source.is_unicast_link_local()
                || definitions(interface, changes).any(|address| address.address == source))
    })
}
pub(super) fn compile<'a>(
    entries: &mut Vec<Entry>,
    batch: RouteConfigurationBatch<'_, Ipv6Addr>,
    interfaces: impl Clone + Iterator<Item = &'a NetworkInterface>,
    changes: &[InterfaceChange],
) -> Result<Option<Arc<ManagedPaths>>, SocketError> {
    let mut groups = nexthop::NextHops::new();
    for definition in batch.next_hops() {
        let group = group::Group::capture(definition)?;
        for path in group.members() {
            member(*path, interfaces.clone(), changes)?;
        }
        groups.define(
            nexthop::Name::new(definition.id).ok_or(SocketError::Invalid)?,
            group,
        )?;
    }
    for (order, input) in batch.iter().enumerate() {
        let (route, prepared) = match input {
            RouteConfiguration::Inline(request) => {
                let route = attributes(request.into(), interfaces.clone(), changes)?;
                if route.kind == RouteKind::Unicast {
                    let path = NextHopMember {
                        interface_index: request.interface_index.ok_or(SocketError::NoDevice)?,
                        gateway: request.gateway,
                        on_link: request.on_link,
                        weight: 1,
                    };
                    member(path, interfaces.clone(), changes)?;
                    let interface = interfaces
                        .clone()
                        .find(|interface| interface.index == path.interface_index)
                        .ok_or(SocketError::NoDevice)?;
                    if route.scope >= 253 && path.gateway.is_some()
                        || !source_scope(interface, route.preferred_source, changes)
                    {
                        return Err(SocketError::Invalid);
                    }
                    (route, Some(groups.prepare(path, None)?))
                } else {
                    if request.interface_index.is_some()
                        || request.gateway.is_some()
                        || request.on_link
                    {
                        return Err(SocketError::Invalid);
                    }
                    (route, None)
                }
            }
            RouteConfiguration::Via { route, next_hop } => {
                let route = attributes(route, interfaces.clone(), changes)?;
                if route.kind != RouteKind::Unicast {
                    return Err(SocketError::Invalid);
                }
                let name = nexthop::Name::new(next_hop).ok_or(SocketError::Invalid)?;
                let group = groups.named_group(name)?;
                for path in group.members() {
                    let interface = interfaces
                        .clone()
                        .find(|interface| interface.index == path.interface_index)
                        .ok_or(SocketError::NoDevice)?;
                    if route.scope >= 253 && path.gateway.is_some()
                        || !source_scope(interface, route.preferred_source, changes)
                    {
                        return Err(SocketError::Invalid);
                    }
                }
                let first = *group.members().first().ok_or(SocketError::Invalid)?;
                (route, Some(groups.prepare(first, Some(name))?))
            }
        };
        let path = prepared.map_or(Path::Reject, |prepared| {
            Path::Managed(groups.acquire(prepared, route.scope))
        });
        entries.push(Entry {
            route,
            path,
            order: order as u32,
        });
    }
    if groups.is_empty() {
        Ok(None)
    } else {
        Arc::try_new(ManagedPaths::new(groups)?)
            .map(Some)
            .map_err(|_| SocketError::NoMemory)
    }
}
