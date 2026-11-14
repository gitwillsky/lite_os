//! IPv6 的单一执行索引；自动地址与受管路由同根发布，不复制 DAD/邻居或按前缀复制路径。
use super::super::{NetworkInterface, configuration::InterfaceChange};
use super::{Ipv6Interfaces, TABLE_LOCAL, TABLE_MAIN, group, nexthop};
use alloc::{sync::Arc, vec::Vec};
use core::net::Ipv6Addr;
use network_protocol::{
    RouteKind, RouteMetrics, SocketError,
    routes::{RouteAttributes, RouteConfigurationBatch},
};
use prefix_index::{Address, Poptrie};
mod admission;
mod observation;
pub(super) use admission::member as admit_member;
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Local(u32),
    Direct(u32),
    Managed(nexthop::Id),
    Reject,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Entry {
    route: RouteAttributes<Ipv6Addr>,
    path: Path,
    // 受管路由在同一前缀内按下发顺序选路。原始 metric 只用于查询，不代表管理距离。
    order: u32,
}

impl Entry {
    fn managed(&self) -> bool {
        matches!(self.path, Path::Managed(_) | Path::Reject)
    }
    fn prefix(&self) -> (u128, u8) {
        (u128::from(self.route.destination), self.route.prefix_length)
    }
    fn table_order(&self) -> u64 {
        match self.route.table {
            TABLE_LOCAL => 0,
            TABLE_MAIN => 1,
            alternate => u64::from(alternate) + 2,
        }
    }
}
enum GatewayChoice {
    Configured,
    Available,
    Due,
}

enum RouteLookup {
    Output(Option<u32>),
    Input(Option<u32>, u64),
}

struct ManagedPaths {
    groups: nexthop::NextHops<Ipv6Addr>,
    // Shared by FIB roots. ND failure and retry look up the gateway without scanning routes.
    gateways: Vec<(u32, Ipv6Addr, nexthop::Id)>,
}

impl ManagedPaths {
    fn new(groups: nexthop::NextHops<Ipv6Addr>) -> Result<Self, SocketError> {
        let gateways = groups.active_groups().flat_map(|(id, members)| {
            members.iter().filter_map(move |member| {
                member
                    .gateway
                    .map(|gateway| (member.interface_index, gateway, id))
            })
        });
        let mut keys = Vec::new();
        keys.try_reserve_exact(gateways.clone().count())
            .map_err(|_| SocketError::NoMemory)?;
        keys.extend(gateways);
        keys.sort_unstable();
        keys.dedup();
        Ok(Self {
            groups,
            gateways: keys,
        })
    }
}

pub(in crate::inet) struct Ipv6Routes {
    entries: Vec<Entry>,
    index: Poptrie<u128>,
    // OWNER: 不可变路径字典在候选/逆向根间共享；接口地址变化不能复制所有组，热路径只借用，不增 Arc 引用。
    // 字典不持在线状态；不复用的 ifindex 与当前设备事实排除已退出路径，配置替换/域退出退休描述。
    groups: Option<Arc<ManagedPaths>>,
    // Index only the groups referenced by this root. Detach and rollback can retire routes
    // while another root still holds the shared path dictionary.
    active: Vec<u64>,
}
#[derive(Clone, Copy)]
pub(in crate::inet) struct Ipv6Selection {
    pub(in crate::inet) interface: u32,
    pub(in crate::inet) local: bool,
    pub(in crate::inet) neighbor: Ipv6Addr,
    pub(in crate::inet) via_gateway: bool,
    pub(in crate::inet) table: u32,
    pub(in crate::inet) preferred_source: Option<Ipv6Addr>,
    pub(in crate::inet) metrics: RouteMetrics,
    pub(in crate::inet) recovery: Option<(u32, Ipv6Addr)>,
}
/// 当前 guard 内的字段借用；RX loan 只借设备，选路不能扩大成整个接口的借用。
#[derive(Clone, Copy)]
pub(in crate::inet) struct Ipv6InterfaceFacts<'a> {
    pub(in crate::inet) index: u32,
    pub(in crate::inet) up: bool,
    pub(in crate::inet) carrier: bool,
    pub(in crate::inet) mtu: usize,
    pub(in crate::inet) addresses: Option<&'a super::super::ipv6::InterfaceAddresses>,
}
impl<'a> Ipv6InterfaceFacts<'a> {
    pub(in crate::inet) fn capture(interface: &'a NetworkInterface) -> Self {
        Self {
            index: interface.index,
            up: interface.state.up,
            carrier: interface.has_carrier(),
            mtu: interface.device.ip_mtu(),
            addresses: interface.ipv6.as_ref(),
        }
    }
    fn usable(self) -> bool {
        self.up
            && self.carrier
            && self.mtu >= 1280
            && self
                .addresses
                .is_some_and(|owner| owner.control_source().is_some())
    }
}
/// 自动组播链路的默认选择；成员与发送共用，读取实际接口而不保存另一个默认设备。
pub(in crate::inet) fn default_multicast_interface<'a>(
    interfaces: impl Iterator<Item = Ipv6InterfaceFacts<'a>>,
) -> Option<u32> {
    interfaces
        .filter(|entry| entry.up && entry.carrier && entry.mtu >= 1280 && entry.addresses.is_some())
        .min_by_key(|entry| (entry.index == 1, entry.index))
        .map(|entry| entry.index)
}
fn definitions<'a>(
    interface: &'a NetworkInterface,
    changes: &'a [InterfaceChange],
) -> impl Iterator<Item = router_protocol::configuration::Ipv6Address> + Clone + 'a {
    let replacement = changes
        .iter()
        .find(|change| change.index == interface.index)
        .and_then(|change| change.ipv6.as_ref());
    interface
        .ipv6
        .iter()
        .flat_map(move |owner| owner.definitions(replacement))
}
fn automatic(
    address: router_protocol::configuration::Ipv6Address,
    index: u32,
    local: bool,
) -> Entry {
    Entry {
        route: RouteAttributes {
            destination: if local {
                address.address
            } else {
                Ipv6Addr::from(u128::from(address.address).masked(address.prefix_length))
            },
            prefix_length: if local { 128 } else { address.prefix_length },
            table: if local { TABLE_LOCAL } else { TABLE_MAIN },
            metric: 0,
            scope: if local { 254 } else { 253 },
            protocol: 2,
            kind: if local {
                RouteKind::Local
            } else {
                RouteKind::Unicast
            },
            preferred_source: None,
            metrics: RouteMetrics::default(),
        },
        path: if local {
            Path::Local(index)
        } else {
            Path::Direct(index)
        },
        order: 0,
    }
}
impl Ipv6Routes {
    pub(in crate::inet) fn prefix_index(&self) -> &Poptrie<u128> {
        &self.index
    }

    /// 候选先于发布准备；base 保留未替换的受管声明，显式空 forwarding 撤回全部受管声明。
    pub(in crate::inet) fn prepare<'a>(
        interfaces: impl Iterator<Item = &'a NetworkInterface> + Clone,
        changes: &[InterfaceChange],
        base: Option<&Self>,
        forwarding: Option<RouteConfigurationBatch<'_, Ipv6Addr>>,
    ) -> Result<Self, SocketError> {
        let mut entries = Vec::new();
        let managed_count = forwarding.as_ref().map_or_else(
            || {
                base.map_or(0, |base| {
                    base.entries.iter().filter(|entry| entry.managed()).count()
                })
            },
            |batch| batch.iter().len(),
        );
        let automatic_count = interfaces.clone().try_fold(0usize, |count, interface| {
            definitions(interface, changes).try_fold(count, |count, address| {
                count
                    .checked_add(1 + usize::from(address.on_link))
                    .ok_or(SocketError::NoMemory)
            })
        })?;
        entries
            .try_reserve_exact(
                automatic_count
                    .checked_add(managed_count)
                    .ok_or(SocketError::NoMemory)?,
            )
            .map_err(|_| SocketError::NoMemory)?;
        for interface in interfaces.clone() {
            for address in definitions(interface, changes) {
                entries.push(automatic(address, interface.index, true));
                if address.on_link {
                    entries.push(automatic(address, interface.index, false));
                }
            }
        }
        let groups = if let Some(batch) = forwarding {
            admission::compile(&mut entries, batch, interfaces, changes)?
        } else {
            if let Some(base) = base {
                entries.extend(base.entries.iter().filter(|entry| entry.managed()).copied());
            }
            base.and_then(|base| base.groups.clone())
        };
        entries.sort_unstable_by_key(|entry| {
            (
                entry.prefix(),
                entry.table_order(),
                match entry.path {
                    Path::Local(_) => 0,
                    Path::Direct(_) => 1,
                    _ => 2,
                },
                match entry.path {
                    Path::Local(index) | Path::Direct(index) => index,
                    _ => entry.order,
                },
            )
        });
        entries.dedup();
        let prefixes = entries.iter().map(Entry::prefix);
        let index = match base {
            Some(base) => base.index.rebuild(prefixes),
            None => Poptrie::build(prefixes),
        }
        .map_err(|error| match error {
            prefix_index::Error::NoMemory => SocketError::NoMemory,
            prefix_index::Error::NoSpace => SocketError::NoSpace,
            prefix_index::Error::Invalid => SocketError::Invalid,
        })?;
        let count = groups
            .as_ref()
            .map_or(0, |paths| paths.groups.slot_count())
            .div_ceil(64);
        let mut active = Vec::new();
        active
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        active.resize(count, 0);
        let mut routes = Self {
            entries,
            index,
            groups,
            active,
        };
        routes.refresh_active();
        Ok(routes)
    }
    pub(in crate::inet) fn remove_interface(&mut self, interfaces: &[NetworkInterface]) {
        self.retain_interfaces(interfaces);
    }
    pub(in crate::inet) fn retain_interfaces(&mut self, interfaces: &[NetworkInterface]) {
        self.entries.retain(|entry| match entry.path {
            Path::Local(index) | Path::Direct(index) => {
                interfaces.iter().any(|interface| interface.index == index)
            }
            Path::Managed(id) => {
                entry.route.preferred_source.is_none_or(|source| {
                    interfaces.iter().any(|interface| {
                        definitions(interface, &[]).any(|address| address.address == source)
                    })
                }) && self
                    .groups
                    .as_ref()
                    .expect("managed path dictionary")
                    .groups
                    .members(id)
                    .iter()
                    .any(|member| {
                        interfaces
                            .iter()
                            .any(|interface| interface.index == member.interface_index)
                    })
            }
            Path::Reject => true,
        });
        self.index.recount(self.entries.iter().map(Entry::prefix));
        self.refresh_active();
        if self.entries.is_empty() {
            self.entries = Vec::new();
        }
    }
    fn refresh_active(&mut self) {
        self.active.fill(0);
        for entry in &self.entries {
            if let Path::Managed(id) = entry.path {
                let slot = id.get() as usize - 1;
                self.active[slot / 64] |= 1 << (slot % 64);
            }
        }
    }

    /// 当前 FIB 是否在此接口引用网关。只读邻居地址与路由身份，不保存可达性副本。
    pub(in crate::inet) fn has_gateway(&self, interface: u32, gateway: Ipv6Addr) -> bool {
        let Some(paths) = &self.groups else {
            return false;
        };
        let key = (interface, gateway);
        let first = paths
            .gateways
            .partition_point(|&(index, address, _)| (index, address) < key);
        paths.gateways[first..]
            .iter()
            .take_while(|&&(index, address, _)| (index, address) == key)
            .any(|&(_, _, id)| {
                let slot = id.get() as usize - 1;
                self.active[slot / 64] & (1 << (slot % 64)) != 0
            })
    }

    /// LPM 与路径资格来自同一 guard；源未知时先冻结源再重选同组，链路源不能跨 zone。
    #[cfg(test)]
    pub(in crate::inet) fn select(
        &self,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        outgoing: Option<u32>,
        now: u64,
        interfaces: &[NetworkInterface],
    ) -> Result<Ipv6Selection, SocketError> {
        self.select_facts(
            source,
            destination,
            outgoing,
            now,
            Ipv6Interfaces::live(interfaces),
        )
    }
    fn preferred_recovery(
        managed: Option<(u8, u32, Ipv6Addr)>,
        learned: Option<(u8, u32, Ipv6Addr)>,
    ) -> Option<(u32, Ipv6Addr)> {
        managed
            .filter(|(length, _, _)| learned.is_none_or(|(other, _, _)| *length >= other))
            .or(learned)
            .map(|(_, index, gateway)| (index, gateway))
    }

    /// 同一 NET guard 的分拆接口借用，支持 RX loan 存续时按真实状态选择另一出口；不捕获在线副本。
    pub(in crate::inet) fn select_facts<'a>(
        &self,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        outgoing: Option<u32>,
        now: u64,
        interfaces: Ipv6Interfaces<'a>,
    ) -> Result<Ipv6Selection, SocketError> {
        self.select_table(
            source,
            destination,
            outgoing,
            now,
            interfaces,
            RouteLookup::Output(None),
        )
    }

    /// 为 IPv6 转发先尝试匹配的备选表，再用普通路径。拒绝结果终止查找。
    pub(in crate::inet) fn select_transit<'a>(
        &self,
        source: Ipv6Addr,
        (destination, probe): (Ipv6Addr, u64),
        (ingress, mark, rules): (u32, u32, &[router_protocol::policy_routing::Rule]),
        now: u64,
        interfaces: Ipv6Interfaces<'a>,
    ) -> Result<Ipv6Selection, SocketError> {
        let outgoing = destination.is_unicast_link_local().then_some(ingress);
        self.select_with_policy(
            (source, destination, outgoing),
            now,
            interfaces,
            (ingress, mark, rules),
            |table| RouteLookup::Input(table, probe),
        )
    }

    /// 查询本机输出策略。组播仍沿既有链路选择。
    pub(in crate::inet) fn select_output<'a>(
        &self,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        outgoing: Option<u32>,
        now: u64,
        (mark, rules): (u32, &[router_protocol::policy_routing::Rule]),
        interfaces: Ipv6Interfaces<'a>,
    ) -> Result<Ipv6Selection, SocketError> {
        if rules.is_empty() || destination.is_multicast() {
            return self.select_facts(source, destination, outgoing, now, interfaces);
        }
        self.select_with_policy(
            (source, destination, outgoing),
            now,
            interfaces,
            (router_protocol::interface::LOOPBACK_INDEX, mark, rules),
            RouteLookup::Output,
        )
    }

    fn select_with_policy<'a>(
        &self,
        (source, destination, outgoing): (Ipv6Addr, Ipv6Addr, Option<u32>),
        now: u64,
        interfaces: Ipv6Interfaces<'a>,
        (ingress, mark, rules): (u32, u32, &[router_protocol::policy_routing::Rule]),
        lookup: impl Fn(Option<u32>) -> RouteLookup,
    ) -> Result<Ipv6Selection, SocketError> {
        let mut matching = rules
            .iter()
            .filter(|rule| super::policy::matches_source(rule, source.into(), ingress, mark));
        if let Some(first) = matching.next()
            // 本机地址在选表规则之前处理。转发报文排队期间也可能新配置本机地址。
            && !interfaces.scoped(outgoing).any(|interface| {
                interface.usable()
                    && interface.addresses.is_some_and(|owner| owner.has_address(destination))
            })
        {
            for rule in core::iter::once(first).chain(matching) {
                let result = self.select_table(
                    source,
                    destination,
                    outgoing,
                    now,
                    interfaces,
                    lookup(Some(rule.table)),
                );
                if !matches!(result, Err(SocketError::NetworkUnreachable)) {
                    return result;
                }
            }
        }
        self.select_table(source, destination, outgoing, now, interfaces, lookup(None))
    }

    fn select_table<'a>(
        &self,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        outgoing: Option<u32>,
        now: u64,
        interfaces: Ipv6Interfaces<'a>,
        lookup: RouteLookup,
    ) -> Result<Ipv6Selection, SocketError> {
        let (table, probe) = match lookup {
            RouteLookup::Output(table) => (table, 0),
            RouteLookup::Input(table, probe) => (table, probe),
        };
        if table.is_none() && destination.is_multicast() {
            if matches!(destination.octets()[1] & 15, 0 | 15) {
                return Err(SocketError::Invalid);
            }
            let index = outgoing
                .or_else(|| default_multicast_interface(interfaces.iter()))
                .ok_or(SocketError::NetworkUnreachable)?;
            if !interfaces
                .get(index)
                .is_some_and(Ipv6InterfaceFacts::usable)
            {
                return Err(SocketError::NetworkUnreachable);
            }
            // 主机组播映射到选中链路的 multicast MAC，不借单播 gateway 发起 ND。
            return Ok(Ipv6Selection {
                interface: index,
                local: false,
                neighbor: destination,
                via_gateway: false,
                table: TABLE_MAIN,
                preferred_source: None,
                metrics: RouteMetrics::default(),
                recovery: None,
            });
        }
        // 通告是有界的动态路由来源。按同一最长前缀顺序与已编译条目竞争，
        // 不把每次续期复制到可能包含完整 BGP 表的索引，也不另建后备路由判定。
        let mut live = None;
        let mut failed = None;
        let mut retry = None;
        if table.is_none() {
            for interface in interfaces
                .scoped(outgoing)
                .filter(|interface| interface.usable())
            {
                let owner = interface.addresses.expect("usable IPv6 interface");
                if source.is_unicast_link_local() && !owner.has_address(source) {
                    continue;
                }
                for route in owner.learned_routes(now).filter(|route| {
                    u128::from(destination).masked(route.length) == u128::from(route.network)
                }) {
                    let (available, resolved) = route
                        .gateway
                        .map_or((true, true), |gateway| owner.router_rank(gateway));
                    let rank = (
                        route.local,
                        route.gateway.is_none(),
                        route.length,
                        core::cmp::Reverse(route.metric),
                        resolved || !available,
                        core::cmp::Reverse(interface.index),
                        core::cmp::Reverse(route.gateway),
                    );
                    let candidate = (rank, interface.index, route);
                    let slot = if available { &mut live } else { &mut failed };
                    if slot.is_none_or(|(previous, _, _)| rank > previous) {
                        *slot = Some(candidate);
                    }
                    if !available
                        && retry.is_none_or(|(previous, _, _)| rank > previous)
                        && route
                            .gateway
                            .is_some_and(|gateway| owner.router_probe_due(gateway, now))
                    {
                        retry = Some(candidate);
                    }
                }
            }
        }
        let recovery = retry.map(|(rank, index, route)| {
            (
                rank,
                index,
                route.gateway.expect("failed route has gateway"),
            )
        });
        let learned_selection = |(rank, index, route): (_, u32, _), managed| {
            let route: super::super::ipv6::LearnedRoute = route;
            Ipv6Selection {
                interface: index,
                local: route.local,
                neighbor: route.gateway.unwrap_or(destination),
                via_gateway: route.gateway.is_some(),
                table: if route.local { TABLE_LOCAL } else { TABLE_MAIN },
                preferred_source: None,
                metrics: RouteMetrics::default(),
                recovery: Self::preferred_recovery(
                    managed,
                    recovery
                        .filter(|(candidate, _, _)| candidate > &rank)
                        .map(|(rank, index, gateway)| (rank.2, index, gateway)),
                ),
            }
        };
        let recovery_for_prefix = |length, managed| {
            Self::preferred_recovery(
                managed,
                recovery
                    .filter(|(rank, _, _)| rank.2 > length)
                    .map(|(rank, index, gateway)| (rank.2, index, gateway)),
            )
        };
        let mut managed_probe = None;
        let mut managed_failed = None;
        let mut managed_retry = None;
        for prefix in self.index.matching_from(u128::from(destination), probe) {
            if let Some(candidate) = live
                && (candidate.2.length > prefix.length
                    || candidate.2.local && candidate.2.length == prefix.length)
            {
                return Ok(learned_selection(candidate, managed_probe));
            }
            let group = &self.entries[prefix.range()];
            let entries = if let Some(table) = table {
                let start =
                    group.partition_point(|entry| entry.table_order() < u64::from(table) + 2);
                &group[start..]
            } else {
                group
            };
            for entry in entries {
                if table.map_or_else(
                    || entry.table_order() > 1,
                    |table| entry.route.table != table,
                ) {
                    break;
                }
                match entry.path {
                    Path::Reject => {
                        return Err(match entry.route.kind {
                            RouteKind::Blackhole => SocketError::Invalid,
                            RouteKind::Prohibit => SocketError::PermissionDenied,
                            _ => SocketError::HostUnreachable,
                        });
                    }
                    Path::Local(index) | Path::Direct(index) => {
                        if outgoing.is_some_and(|bound| bound != index) {
                            continue;
                        }
                        let Some(interface) = interfaces.get(index) else {
                            continue;
                        };
                        if !interface.usable() {
                            continue;
                        }
                        let local = matches!(entry.path, Path::Local(_));
                        if local
                            && !interface
                                .addresses
                                .is_some_and(|owner| owner.has_address(destination))
                        {
                            continue;
                        }
                        return Ok(Ipv6Selection {
                            interface: index,
                            local,
                            neighbor: destination,
                            via_gateway: false,
                            table: entry.route.table,
                            preferred_source: None,
                            metrics: entry.route.metrics,
                            recovery: recovery_for_prefix(prefix.length, managed_probe),
                        });
                    }
                    Path::Managed(id) => {
                        let dictionary = self.groups.as_ref().expect("managed path dictionary");
                        let (members, name) = dictionary.groups.view(id);
                        let seed = name.map_or_else(|| u64::from(id.get()), |name| name.get());
                        let owner = |index, bound: Option<u32>, source: Ipv6Addr| {
                            if bound.is_some_and(|bound| index != bound) {
                                return None;
                            }
                            let interface = interfaces
                                .get(index)
                                .filter(|interface| interface.usable())?;
                            let owner = interface.addresses?;
                            (!source.is_unicast_link_local() || owner.has_address(source))
                                .then_some(owner)
                        };
                        let choose = |source, bound, mode| {
                            group::select_members(
                                members,
                                || group::hash_ipv6(source, destination, seed),
                                |index| owner(index, bound, source),
                                |path, owner| match mode {
                                    GatewayChoice::Configured => true,
                                    GatewayChoice::Available => path
                                        .gateway
                                        .is_none_or(|gateway| owner.router_rank(gateway).0),
                                    GatewayChoice::Due => path.gateway.is_some_and(|gateway| {
                                        owner.router_probe_due(gateway, now)
                                    }),
                                },
                            )
                        };
                        let available = |member: nexthop::NextHop<Ipv6Addr>, bound, source| {
                            member.gateway.is_none_or(|gateway| {
                                owner(member.interface_index, bound, source)
                                    .is_some_and(|owner| owner.router_rank(gateway).0)
                            })
                        };
                        let mut flow_source = source;
                        let mut bound = outgoing;
                        let Some(mut preferred) =
                            choose(flow_source, bound, GatewayChoice::Configured)
                        else {
                            continue;
                        };
                        let mut preferred_source = entry.route.preferred_source;
                        if members.len() > 1
                            && source.is_unspecified()
                            && matches!(lookup, RouteLookup::Output(_))
                        {
                            let provisional = if available(preferred, bound, source) {
                                preferred
                            } else {
                                choose(source, bound, GatewayChoice::Available).unwrap_or(preferred)
                            };
                            if let Some(chosen) = preferred_source.or_else(|| {
                                owner(provisional.interface_index, bound, source)?
                                    .source_for(destination)
                            }) {
                                if chosen.is_unicast_link_local() {
                                    bound = Some(provisional.interface_index);
                                }
                                flow_source = chosen;
                                preferred = choose(flow_source, bound, GatewayChoice::Configured)
                                    .expect("provisional IPv6 path supplies a usable source");
                                preferred_source = Some(chosen);
                            }
                        }
                        let selection = |member: nexthop::NextHop<Ipv6Addr>| Ipv6Selection {
                            interface: member.interface_index,
                            local: false,
                            neighbor: member.gateway.unwrap_or(destination),
                            via_gateway: member.gateway.is_some(),
                            table: entry.route.table,
                            preferred_source,
                            metrics: entry.route.metrics,
                            recovery: None,
                        };
                        if available(preferred, bound, flow_source) {
                            return Ok(Ipv6Selection {
                                recovery: recovery_for_prefix(prefix.length, managed_probe),
                                ..selection(preferred)
                            });
                        }

                        let gateway = preferred.gateway.expect("unavailable member is a gateway");
                        if owner(preferred.interface_index, bound, flow_source)
                            .is_some_and(|owner| owner.router_probe_due(gateway, now))
                        {
                            managed_probe.get_or_insert((
                                prefix.length,
                                preferred.interface_index,
                                gateway,
                            ));
                        }
                        if let Some(member) = choose(flow_source, bound, GatewayChoice::Available) {
                            return Ok(Ipv6Selection {
                                recovery: recovery_for_prefix(prefix.length, managed_probe),
                                ..selection(member)
                            });
                        }
                        managed_failed.get_or_insert_with(|| (prefix.length, selection(preferred)));
                        if let Some(member) = choose(flow_source, bound, GatewayChoice::Due) {
                            let gateway = member.gateway.expect("probe candidate is a gateway");
                            managed_probe.get_or_insert((
                                prefix.length,
                                member.interface_index,
                                gateway,
                            ));
                            managed_retry.get_or_insert_with(|| (prefix.length, selection(member)));
                        }
                    }
                }
            }
        }
        if let Some(candidate) = live {
            return Ok(learned_selection(candidate, managed_probe));
        }
        if let Some((length, selection)) = managed_retry
            && retry.is_none_or(|(_, _, route)| length >= route.length)
        {
            return Ok(selection);
        }
        if let Some(candidate) = retry {
            return Ok(learned_selection(candidate, None));
        }
        if let Some((length, selection)) = managed_failed
            && failed.is_none_or(|(_, _, route)| length >= route.length)
        {
            return Ok(selection);
        }
        failed
            .map(|candidate| learned_selection(candidate, None))
            .ok_or(SocketError::NetworkUnreachable)
    }
}
