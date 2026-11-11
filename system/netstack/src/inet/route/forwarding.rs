//! 原生受管 FIB：完整集合与独立具名下一跳更新均经过原准入和配置会话。
use super::configuration::{configuration_facts, replaced};
use super::*;
use crate::inet::configuration::InterfaceChange;
use network_protocol::routes::{NextHopConfiguration, RouteAttributes, RouteConfiguration};

fn route_error(error: Ipv4RouteError) -> SocketError {
    match error {
        Ipv4RouteError::NoMemory => SocketError::NoMemory,
        Ipv4RouteError::NoDevice => SocketError::NoDevice,
        Ipv4RouteError::NetworkDown => SocketError::NetworkDown,
        Ipv4RouteError::NetworkUnreachable => SocketError::NetworkUnreachable,
        Ipv4RouteError::Again => SocketError::Again,
        _ => SocketError::Invalid,
    }
}
impl RoutingTable {
    /// 下一跳只按邻接/本机路由验证；不能借即将撤销的受管 FIB 证明自己有效。
    pub(super) fn admit_next_hop(
        &self,
        definition: NextHopConfiguration<'_, core::net::Ipv4Addr>,
        interfaces: &[NetworkInterface],
    ) -> Result<group::Group<Ipv4Addr>, SocketError> {
        Self::admit_group(definition, |request| {
            self.admit(request, interfaces, Origin::Forwarding)
        })
    }
    fn admit_group(
        definition: NextHopConfiguration<'_, core::net::Ipv4Addr>,
        admit: impl Fn(Ipv4RouteRequest) -> Result<Record, Ipv4RouteError>,
    ) -> Result<group::Group<Ipv4Addr>, SocketError> {
        if definition.id == 0 || definition.is_empty() {
            return Err(SocketError::Invalid);
        }
        let group = group::Group::capture(definition)?;
        for member in group.members() {
            admit(member.request(RouteAttributes {
                destination: Ipv4Addr::UNSPECIFIED,
                prefix_length: 0,
                table: TABLE_MAIN,
                metric: 0,
                scope: 0,
                protocol: 0,
                kind: RouteKind::Unicast,
                preferred_source: None,
                metrics: RouteMetrics::default(),
            }))
            .map_err(route_error)?;
        }
        Ok(group)
    }
    /// 同批准备接口邻接及完整受管路由/具名定义；其他来源仍按同一候选接口事实保留或退休。
    pub(in crate::inet) fn prepare_routes<'a>(
        &self,
        definitions: impl ExactSizeIterator<Item = NextHopConfiguration<'a, core::net::Ipv4Addr>>,
        routes: impl ExactSizeIterator<Item = RouteConfiguration<core::net::Ipv4Addr>>,
        interfaces: &[NetworkInterface],
        changes: &[InterfaceChange],
    ) -> Result<(PreparedRouteChange, Vec<u32>), SocketError> {
        let adjacency = Self::configuration_draft(changes)?;
        let facts = configuration_facts(interfaces, changes);
        let admit = |request| {
            Self::admit_with(
                request,
                facts.clone(),
                Origin::Forwarding,
                (interfaces, changes),
                |destination, bound, minimum_scope| {
                    self.configuration_lookup(
                        Flow {
                            source: Ipv4Addr::UNSPECIFIED,
                            destination,
                            bound,
                        },
                        minimum_scope,
                        &adjacency,
                        changes,
                        interfaces,
                        true,
                    )
                },
            )
        };
        let mut candidate = Draft::new(routes.len())?;
        let mut dependencies = Vec::new();
        dependencies
            .try_reserve_exact(interfaces.len())
            .map_err(|_| SocketError::NoMemory)?;
        for definition in definitions {
            let group = Self::admit_group(definition, admit)?;
            for member in group.members() {
                if !dependencies.contains(&member.interface_index) {
                    dependencies.push(member.interface_index);
                }
            }
            candidate.define_group(definition.id, group)?;
        }
        let order = -i128::from(crate::identity::next());
        for route in routes {
            let record = match route {
                RouteConfiguration::Inline(request) => admit(request).map_err(route_error)?,
                RouteConfiguration::Via { route, next_hop } => {
                    let name = nexthop::Name::new(next_hop).ok_or(SocketError::Invalid)?;
                    let group = candidate.named(name)?;
                    if route.scope >= 254 && group.members().len() != 1 {
                        return Err(SocketError::Invalid);
                    }
                    let mut first = None;
                    for member in group.members() {
                        let record = admit(member.request(route)).map_err(route_error)?;
                        first.get_or_insert(record);
                    }
                    let mut record = first.ok_or(SocketError::NoDevice)?;
                    record.next_hop_name = Some(name);
                    record
                }
            };
            if let Some(index) = record.interface_index
                && !dependencies.contains(&index)
            {
                dependencies.push(index);
            }
            candidate.push(record, order)?;
        }
        if !candidate.unique_prefixes() {
            return Err(SocketError::Invalid);
        }
        candidate.extend(adjacency)?;
        let prepared = self.prepare_change(
            candidate,
            |record| record.origin == Origin::Forwarding || replaced(record, changes, &self.fib),
            interfaces,
            facts,
        )?;
        Ok((prepared, dependencies))
    }
}
