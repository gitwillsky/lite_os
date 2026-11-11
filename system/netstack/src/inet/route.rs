//! 一个域的实际IPv4 FIB；所有前缀/metric/自动与用户路由共享同一记录和查找。
use super::{InterfaceState, interface::NetworkInterface};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
use network_protocol::{Ipv4RouteError, Ipv4RouteRequest, RouteKind, RouteMetrics, SocketError};

mod output;
mod policy;
pub(super) use output::{Output, output_for, probe_gateway};
pub(super) use policy::SelectionRules;
mod ipv6;
pub(super) use ipv6::{Ipv6InterfaceFacts, Ipv6Routes, Ipv6Selection, default_multicast_interface};
mod admission;
mod configuration;
mod fib;
mod forwarding;
use fib::{Draft, Entry, Fib};
mod group;
mod nexthop;
pub(super) use configuration::PreparedRouteChange;
use prefix_index as poptrie;

pub(super) use router_protocol::policy_routing::{TABLE_LOCAL, TABLE_MAIN};

/// 把查找终止动作和表未命中投影为原生错误。
pub(super) fn selection_error(kind: RouteKind) -> SocketError {
    match kind {
        RouteKind::Unreachable => SocketError::HostUnreachable,
        RouteKind::Blackhole => SocketError::Invalid,
        RouteKind::Prohibit => SocketError::PermissionDenied,
        _ => SocketError::NetworkUnreachable,
    }
}

/// 路由的传输参数：未配置的 advertised MSS 按出口 MTU 推导。
fn transport(metrics: RouteMetrics, mtu: usize) -> smoltcp::iface::IpRouteContext {
    smoltcp::iface::IpRouteContext {
        mtu,
        advertised_mss: if metrics.advertised_mss == 0 {
            mtu.saturating_sub(40).clamp(256, 65535 - 40)
        } else {
            metrics.advertised_mss as usize
        },
        receive_window: metrics.receive_window as usize,
        initial_rtt: metrics.round_trip_time,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Interface(u32),
    User,
    Forwarding,
}
/// 一次冷准入或读取投影；不作为常驻 FIB 存储，不复制设备寿命。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    request: Ipv4RouteRequest,
    origin: Origin,
    next_hop_name: Option<nexthop::Name>,
    // 已解析的出口投影；实际绑定由 FIB 下一跳拥有，lookup 仍读取当前设备状态。
    interface_index: Option<u32>,
}
/// 一次实际FIB查找结果；出口和logical iif分开，以保留原生LOCAL物理oif经lo交付。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RouteSelection {
    pub(super) interface_index: u32,
    pub(super) logical_interface_index: u32,
    pub(super) preferred_source: Option<Ipv4Addr>,
    pub(super) next_hop: IpAddr,
    pub(super) via_gateway: bool,
    pub(super) table: u32,
    pub(super) kind: RouteKind,
    pub(super) transport: smoltcp::iface::IpRouteContext,
}
/// 一次 L3 流查找键；明确出口只收紧同一组，不创建旁路 selector。
#[derive(Clone, Copy)]
pub(super) struct Flow {
    pub(super) source: Ipv4Addr,
    pub(super) destination: Ipv4Addr,
    pub(super) bound: Option<u32>,
}
/// 本次guard内的设备窄投影；iterator即时读取actual owner，不持久复制配置。
#[derive(Clone, Copy)]
pub(super) struct InterfaceFacts {
    pub(super) index: u32,
    pub(super) address: Option<Ipv4Addr>,
    pub(super) up: bool,
    pub(super) mtu: usize,
}
impl InterfaceFacts {
    pub(super) fn capture(interface: &NetworkInterface) -> Self {
        Self {
            index: interface.index,
            address: interface.state.address,
            up: interface.state.up,
            mtu: interface.device.ip_mtu(),
        }
    }
}
/// 唯一活动 FIB；连续记录按 prefix/table/metric/publication 排列，索引直接引用自己的 alias 区间。
pub(super) struct RoutingTable {
    fib: Fib,
}
pub(super) struct PreparedInterfaceRoutes {
    fib: Fib,
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn source_for_ipv4_path(
    index: u32,
    gateway: Option<IpAddr>,
    interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
) -> Option<Ipv4Addr> {
    interfaces
        .clone()
        .find(|interface| interface.index == index)
        .and_then(|interface| interface.address)
        .or_else(|| {
            // IPv6-only 出口可发送本域其他接口的 IPv4 地址；链路本地和回环地址不能跨接口使用。
            gateway.filter(IpAddr::is_ipv6)?;
            interfaces
                .filter(|interface| interface.up)
                .find_map(|interface| {
                    interface
                        .address
                        .filter(|address| !address.is_loopback() && !address.is_link_local())
                })
        })
}
impl RoutingTable {
    pub(super) fn has_gateway(&self, index: u32, gateway: IpAddr) -> bool {
        self.fib.has_gateway(index, gateway)
    }
    pub(super) fn new() -> Self {
        Self { fib: Fib::new() }
    }
    /// 预备接口与相关路由；force 用于 MTU 失去 IPv4 能力时强制退休绑定，即使地址此前已为空。
    /// 缺少强制退休会让无地址设备上的静态成员越过 MTU 下限继续存活。
    pub(super) fn prepare_interface(
        &self,
        index: u32,
        previous: InterfaceState,
        state: InterfaceState,
        interfaces: &[NetworkInterface],
        force: bool,
    ) -> Result<PreparedInterfaceRoutes, SocketError> {
        let mut entries = Self::interface_entries(index, state)?;
        entries.inherit_named(&self.fib)?;
        if force || previous.address != state.address {
            entries.retire_interface(index, true);
        } else if previous.up && !state.up {
            entries.retire_interface(index, false);
        }
        let fib = self.fib.prepare(entries, |record| {
            record.origin == Origin::Interface(index)
                || !self.fib.is_multipath(record.next_hop_name)
                    && record.interface_index == Some(index)
                    && (force
                        || previous.address != state.address
                        || previous.up && !state.up && record.request.scope != 254)
        })?;
        let mut candidate = Self { fib };
        candidate.validate_sources(interfaces.iter().map(|interface| {
            let selected = if interface.index == index {
                state
            } else {
                interface.state
            };
            InterfaceFacts {
                index: interface.index,
                address: selected.address,
                up: selected.up,
                mtu: interface.device.ip_mtu(),
            }
        }))?;
        Ok(PreparedInterfaceRoutes { fib: candidate.fib })
    }
    fn interface_entries(index: u32, state: InterfaceState) -> Result<Draft, SocketError> {
        let mut entries = Draft::new(usize::from(state.address.is_some()) * 4)?;
        let Some(address) = state.address else {
            return Ok(entries);
        };
        let mut append = |destination: Ipv4Addr,
                          prefix_length: u8,
                          kind: RouteKind|
         -> Result<(), SocketError> {
            let table = if kind == RouteKind::Unicast {
                TABLE_MAIN
            } else {
                TABLE_LOCAL
            };
            let request = Ipv4RouteRequest {
                destination,
                prefix_length,
                table,
                metric: 0,
                scope: if kind == RouteKind::Local { 254 } else { 253 },
                protocol: 2,
                kind,
                interface_index: Some(index),
                gateway: None,
                preferred_source: Some(address),
                on_link: false,
                metrics: RouteMetrics::default(),
            };
            // explicit broadcast等于prefix broadcast时native FIB exact alias只保留一次。
            if entries.iter().any(|record| record.request == request) {
                return Ok(());
            }
            let order = crate::identity::next();
            let record = Record {
                next_hop_name: None,
                request,
                origin: Origin::Interface(index),
                interface_index: Some(index),
            };
            entries.push(record, i128::from(order))?;
            Ok(())
        };
        // 1. fib_add_ifaddr在IFF_UP检查之前创建LOCAL /32；DOWN不删除scope HOST的本地成员。
        append(address, 32, RouteKind::Local)?;
        if state.up {
            // 2. explicit与prefix broadcast可同时存在；limited broadcast不建立独立record。
            if !state.broadcast.is_unspecified() && !state.broadcast.is_broadcast() {
                append(state.broadcast, 32, RouteKind::Broadcast)?;
            }
            let network = u32::from(address) & mask(state.prefix_length);
            if network >> 24 != 0 && (network != u32::from(address) || state.prefix_length < 32) {
                append(
                    Ipv4Addr::from(network),
                    state.prefix_length,
                    if index == 1 {
                        RouteKind::Local
                    } else {
                        RouteKind::Unicast
                    },
                )?;
                if state.prefix_length < 31 {
                    append(
                        Ipv4Addr::from(network | !mask(state.prefix_length)),
                        32,
                        RouteKind::Broadcast,
                    )?;
                }
            }
        }
        Ok(entries)
    }
    /// 只交换预备根；所有失败与容量准备都发生在实际接口状态发布前。
    pub(super) fn replace_interface(&mut self, prepared: PreparedInterfaceRoutes) {
        self.fib = prepared.fib;
    }
    fn matching<'a>(
        &self,
        table: u32,
        flow: Flow,
        candidates: impl Iterator<Item = &'a [Entry]>,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
        include: impl Fn(&Record) -> bool,
    ) -> Option<Record> {
        for group in candidates {
            let start = group.partition_point(|entry| entry.table() < table);
            for entry in &group[start..] {
                if entry.table() != table {
                    break;
                }
                let Some(record) = self.fib.select_record(entry, flow, interfaces.clone()) else {
                    continue;
                };
                let eligible = include(&record) && record.request.scope >= minimum_scope;
                if eligible {
                    return Some(record);
                }
            }
        }
        None
    }
    fn lookup_record(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        bound: Option<u32>,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
        include: impl Fn(&Record) -> bool,
    ) -> Option<Record> {
        self.lookup_tables(
            Flow {
                source,
                destination,
                bound,
            },
            minimum_scope,
            interfaces,
            include,
            None,
        )
    }

    fn lookup_tables(
        &self,
        flow: Flow,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
        include: impl Fn(&Record) -> bool,
        policy: Option<(u32, u32, &[router_protocol::policy_routing::Rule])>,
    ) -> Option<Record> {
        let candidates = self.fib.matching(flow.destination);
        let policy = policy.into_iter().flat_map(|(ingress, mark, rules)| {
            rules
                .iter()
                .filter(move |rule| {
                    (!flow.source.is_unspecified() || rule.prefix_length == 0)
                        && rule.matches(flow.source.into(), ingress, mark)
                })
                .map(|rule| rule.table)
        });
        for table in core::iter::once(TABLE_LOCAL)
            .chain(policy)
            .chain(core::iter::once(TABLE_MAIN))
        {
            if let Some(record) = self.matching(
                table,
                flow,
                candidates.clone(),
                minimum_scope,
                interfaces.clone(),
                &include,
            ) && record.request.kind != RouteKind::Throw
            {
                return Some(record);
            }
        }
        None
    }
    /// NETDEV_UNREGISTER：退休该接口的地址routes与全部用户routes。
    pub(super) fn remove_interface(&mut self, index: u32) {
        self.fib.retire_interface(index, true);
    }
    pub(super) fn snapshot_bytes(
        &self,
        interfaces: &[NetworkInterface],
    ) -> Result<Vec<u8>, SocketError> {
        self.fib.snapshot_bytes(interfaces)
    }

    /// 原生单路径配置先验证实际 device/gateway/prefsrc，再准备记录及共享下一跳；失败不改原 FIB。
    pub(super) fn insert(
        &mut self,
        request: Ipv4RouteRequest,
        interfaces: &[NetworkInterface],
        replace: bool,
        create: bool,
        exclusive: bool,
        append: bool,
    ) -> Result<(), Ipv4RouteError> {
        let record = self.admit(request, interfaces, Origin::User)?;
        self.fib.insert(
            record,
            network_protocol::RouteChange {
                delete: false,
                replace,
                create,
                exclusive,
                append,
            },
        )
    }
    /// 原生删除指定前缀与可选 alias 属性；不分配内存。
    pub(super) fn remove(&mut self, request: Ipv4RouteRequest) -> Result<(), Ipv4RouteError> {
        if request.prefix_length > 32
            || u32::from(request.destination) & mask(request.prefix_length)
                != u32::from(request.destination)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        self.fib.remove(request)
    }

    /// 原生最长前缀/metric与LOCAL→lo选择；oif/direct fallback由同一个flow入口处理。
    pub(super) fn select_facts(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        bound: Option<u32>,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, RouteKind> {
        self.select(
            Flow {
                source,
                destination,
                bound,
            },
            interfaces,
            None,
        )
    }

    /// 为跨接口报文应用实际入接口的规则。
    pub(super) fn select_transit(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        (ingress, mark, rules): (u32, u32, &[router_protocol::policy_routing::Rule]),
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, RouteKind> {
        self.select(
            Flow {
                source,
                destination,
                bound: None,
            },
            interfaces,
            Some((ingress, mark, rules)),
        )
    }

    /// 为本机单播查询回环身份对应的规则；组播和有限广播保留链路输出语义。
    pub(super) fn select_output(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        bound: Option<u32>,
        (mark, rules): (u32, &[router_protocol::policy_routing::Rule]),
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, RouteKind> {
        let policy =
            (!rules.is_empty() && !destination.is_multicast() && !destination.is_broadcast())
                .then_some((router_protocol::interface::LOOPBACK_INDEX, mark, rules));
        self.select(
            Flow {
                source,
                destination,
                bound,
            },
            interfaces,
            policy,
        )
    }

    fn select(
        &self,
        flow: Flow,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
        policy: Option<(u32, u32, &[router_protocol::policy_routing::Rule])>,
    ) -> Result<RouteSelection, RouteKind> {
        let Flow {
            destination, bound, ..
        } = flow;
        // limited broadcast/本地组播的明确oif绕过FIB gateway；原生route_output先处理。
        let local_multicast = destination.octets()[..3] == [224, 0, 0];
        if bound.is_some() && (destination.is_broadcast() || local_multicast) {
            return Self::direct(destination, bound, interfaces);
        }
        if let Some(record) = self.lookup_tables(flow, 0, interfaces.clone(), |_| true, policy) {
            let kind = record.request.kind;
            if matches!(
                kind,
                RouteKind::Unreachable | RouteKind::Blackhole | RouteKind::Prohibit
            ) {
                // 原生explicit oif在FIB失败时使用on-link fallback，不能让kernel另写selector。
                return if bound.is_some()
                    && matches!(record.request.table, TABLE_LOCAL | TABLE_MAIN)
                {
                    Self::direct(destination, bound, interfaces)
                } else {
                    Err(kind)
                };
            }
            let logical = record
                .interface_index
                .expect("forwarding route must own actual device");
            let output = if kind == RouteKind::Local { 1 } else { logical };
            let Some(output_interface) = interfaces
                .clone()
                .find(|interface| interface.index == output && interface.up)
            else {
                return Err(RouteKind::Throw);
            };
            let gateway = if destination.is_broadcast() || destination.is_multicast() {
                None
            } else {
                record.request.gateway
            };
            return Ok(RouteSelection {
                interface_index: output,
                logical_interface_index: logical,
                preferred_source: record.request.preferred_source.or_else(|| {
                    if kind == RouteKind::Local {
                        Some(destination)
                    } else {
                        source_for_ipv4_path(logical, record.request.gateway, interfaces.clone())
                    }
                }),
                next_hop: gateway.unwrap_or_else(|| destination.into()),
                via_gateway: gateway.is_some(),
                table: record.request.table,
                kind: if destination.is_broadcast() {
                    RouteKind::Broadcast
                } else {
                    kind
                },
                transport: transport(record.request.metrics, output_interface.mtu),
            });
        }
        // 没有matching FIB只允许实际明确oif的on-link发送；无oif不伪造default。
        Self::direct(destination, bound, interfaces)
    }
    fn direct(
        destination: Ipv4Addr,
        bound: Option<u32>,
        mut interfaces: impl Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, RouteKind> {
        if let Some(index) = bound
            && let Some(interface) = interfaces
                .find(|interface| interface.index == index && interface.up && interface.mtu >= 68)
        {
            return Ok(RouteSelection {
                interface_index: index,
                logical_interface_index: index,
                preferred_source: interface.address,
                next_hop: destination.into(),
                via_gateway: false,
                table: TABLE_MAIN,
                kind: if destination.is_broadcast() {
                    RouteKind::Broadcast
                } else {
                    RouteKind::Unicast
                },
                transport: transport(RouteMetrics::default(), interface.mtu),
            });
        }
        Err(RouteKind::Throw)
    }
    /// 实际LOCAL成员用于bind与prefsrc；DOWN保留HOST record，不伪造127/8成员。
    pub(super) fn is_local(&self, address: Ipv4Addr, interfaces: &[NetworkInterface]) -> bool {
        self.is_local_facts(address, interfaces.iter().map(InterfaceFacts::capture))
    }
    fn is_local_facts(
        &self,
        address: Ipv4Addr,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> bool {
        self.lookup_record(Ipv4Addr::UNSPECIFIED, address, None, 0, interfaces, |_| {
            true
        })
        .is_some_and(|record| record.request.kind == RouteKind::Local)
    }
    /// 发布前按最终候选核对 prefsrc；只有真实失效时申请一位/条的标记，不存另一份本地地址表。
    fn validate_sources(
        &mut self,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<(), SocketError> {
        let mut marks = Vec::new();
        let mut retired = 0;
        for (position, record) in self.fib.iter().enumerate() {
            if record.request.preferred_source.is_some_and(|source| {
                !source.is_unspecified() && !self.is_local_facts(source, interfaces.clone())
            }) {
                if marks.is_empty() {
                    let words = self.fib.iter().len().div_ceil(64);
                    marks
                        .try_reserve_exact(words)
                        .map_err(|_| SocketError::NoMemory)?;
                    marks.resize(words, 0_u64);
                }
                marks[position / 64] |= 1_u64 << (position % 64);
                retired += 1;
            }
        }
        if retired != 0 {
            self.fib.retire_source_marks(&marks, retired)?;
        }
        Ok(())
    }
    /// 按唯一 FIB 分类本次目的；不是第二份本机地址或转发策略表。
    pub(super) fn destination_kind(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<RouteKind> {
        self.lookup_record(source, destination, None, 0, interfaces, |_| true)
            .map(|record| record.request.kind)
    }
    /// ingress 的目的分类来自同一 NET guard 的 destination_kind，避免新转发入口使本机收包重复查找；其余事实在 RX 入队冻结。
    pub(super) fn ingress(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        (index, kind): (u32, Option<RouteKind>),
        path_mtu: Option<(&super::path_mtu::PathMtu, u64)>,
        rules: &[router_protocol::policy_routing::Rule],
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> smoltcp::iface::Ipv4IngressFacts {
        let broadcast = destination.is_broadcast() || kind == Some(RouteKind::Broadcast);
        let local = kind == Some(RouteKind::Local);
        // 一次reverse查找同时投影spec_dst/MTU；同设备bound只收紧同一匹配，
        // 在同guard无mutation下可直接消费这个结论，不重复扫描相同FIB。
        let reverse = self
            .select_output(destination, source, None, (0, rules), interfaces.clone())
            .ok();
        let specific_destination = if broadcast || destination.is_multicast() {
            reverse
                .and_then(|route| route.preferred_source)
                .or_else(|| {
                    interfaces
                        .clone()
                        .find(|interface| interface.index == index)
                        .and_then(|interface| interface.address)
                })
        } else {
            Some(destination)
        };
        let bound_reverse = match reverse {
            Some(route) if route.logical_interface_index == index => Some(route),
            _ => self
                .select_output(destination, source, Some(index), (0, rules), interfaces.clone())
                .ok(),
        };
        let transport = |route| Self::reverse_transport(route, destination, source, path_mtu);
        let reverse_route = reverse.map(transport);
        let bound_reverse_route = bound_reverse.map(transport);
        smoltcp::iface::Ipv4IngressFacts {
            destination,
            interface_index: index,
            local,
            broadcast,
            specific_destination,
            reverse_route,
            bound_reverse_route,
        }
    }

    /// 将反向选路的链路参数与同一次收包时刻的路径 MTU 合并。
    pub(super) fn reverse_transport(
        route: RouteSelection,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        path_mtu: Option<(&super::path_mtu::PathMtu, u64)>,
    ) -> smoltcp::iface::IpRouteContext {
        let mut transport = route.transport;
        if let Some((paths, now)) = path_mtu {
            transport.mtu = paths.get(
                super::path_mtu::Path {
                    interface: route.logical_interface_index,
                    source: source.into(),
                    destination: destination.into(),
                },
                transport.mtu,
                now,
            );
        }
        transport
    }
}
