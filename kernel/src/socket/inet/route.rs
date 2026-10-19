//! 一个NET的实际IPv4 FIB；所有前缀/metric/自动与用户路由共享同一记录和查找。
use super::{InterfaceState, interface::NetworkInterface};
use crate::fallible_tree::{FallibleMap, VacantEntry};
use crate::socket::SocketError;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

pub(super) const TABLE_MAIN: u32 = 254;
pub(super) const TABLE_LOCAL: u32 = 255;

/// FIB实际记录的原生RTAX标量；零表示未配置，RTT单位为八分之一毫秒。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ipv4RouteMetrics {
    pub(crate) advertised_mss: u32,
    pub(crate) receive_window: u32,
    pub(crate) round_trip_time: u32,
}
impl Ipv4RouteMetrics {
    fn transport(self, mtu: usize) -> smoltcp::iface::IpRouteContext {
        smoltcp::iface::IpRouteContext {
            mtu,
            advertised_mss: if self.advertised_mss == 0 {
                mtu.saturating_sub(40).clamp(256, 65535 - 40)
            } else {
                self.advertised_mss as usize
            },
            receive_window: self.receive_window as usize,
            initial_rtt: self.round_trip_time,
        }
    }
}
/// 原生路由类型的领域语义；数值由syscall/netlink codec拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ipv4RouteKind {
    Unicast,
    Local,
    Broadcast,
    Unreachable,
    Throw,
}
/// 已解码的标准路由配置；没有raw sockaddr/用户指针或新的userspace ABI。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ipv4RouteRequest {
    pub(crate) destination: Ipv4Addr,
    pub(crate) prefix_length: u8,
    pub(crate) table: u32,
    pub(crate) metric: u32,
    pub(crate) scope: u8,
    pub(crate) protocol: u8,
    pub(crate) kind: Ipv4RouteKind,
    pub(crate) interface_index: Option<u32>,
    pub(crate) gateway: Option<Ipv4Addr>,
    pub(crate) preferred_source: Option<Ipv4Addr>,
    pub(crate) on_link: bool,
    pub(crate) metrics: Ipv4RouteMetrics,
}
/// 配置错误对应原生FIB准入，不借socket的transport生命周期表达ESRCH/EEXIST。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ipv4RouteError {
    Invalid,
    NoDevice,
    NetworkDown,
    NetworkUnreachable,
    NoMemory,
    Exists,
    NotFound,
    Missing,
}

impl Ipv4RouteError {
    /// 对应的 Linux errno（正值）；ioctl 与 rtnetlink 共用，保证两条配置路径报告同一错误。
    pub(crate) const fn errno(self) -> i32 {
        match self {
            Self::Invalid => 22,             // EINVAL
            Self::NoDevice => 19,            // ENODEV
            Self::NetworkDown => 100,        // ENETDOWN
            Self::NetworkUnreachable => 101, // ENETUNREACH
            Self::NoMemory => 12,            // ENOMEM
            Self::Exists => 17,              // EEXIST
            Self::NotFound => 3,             // ESRCH
            Self::Missing => 2,              // ENOENT
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Interface(u32),
    User,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    request: Ipv4RouteRequest,
    origin: Origin,
    // OWNER: gateway publication冻结实际nexthop设备；lookup仍读当前设备状态。
    interface_index: Option<u32>,
}
// prefix属于bucket；同network/table按metric及publication顺序选alias，不能按socket顺序覆盖。
type Key = (u32, u32, u32, i128);
type Prepared = VacantEntry<Key, Record>;

/// 一次实际FIB查找结果；出口和logical iif分开，以保留原生LOCAL物理oif经lo交付。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RouteSelection {
    pub(super) interface_index: u32,
    pub(super) logical_interface_index: u32,
    pub(super) preferred_source: Option<Ipv4Addr>,
    pub(super) next_hop: Ipv4Addr,
    pub(super) kind: Ipv4RouteKind,
    pub(super) transport: smoltcp::iface::IpRouteContext,
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
/// 同一NET唯一FIB；33个prefix roots零分配，实际records各有唯一fallible节点。
pub(super) struct RoutingTable {
    // OWNER: table/network/metric/order alias索引；查找至多33个prefix，不扫描全部用户routes。
    prefixes: [FallibleMap<Key, Record>; 33],
    // OWNER: prefix roots的精确非空索引，与节点mutation同一FIB事务；省略空prefix树遍历。
    // 位只来自实际bucket成员，不缓存route决定；若不在删除最后节点时清除会扫描退休bucket。
    used_prefixes: u64,
}
/// 地址变更在移交actual interface之前完成全部route节点准备；失败无半次FIB publication。
pub(super) struct PreparedInterfaceRoutes {
    entries: Vec<(u8, Prepared)>,
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}
impl RoutingTable {
    pub(super) fn new() -> Self {
        Self {
            prefixes: core::array::from_fn(|_| FallibleMap::new()),
            used_prefixes: 0,
        }
    }
    pub(super) fn prepare_interface(
        index: u32,
        state: InterfaceState,
    ) -> Result<PreparedInterfaceRoutes, SocketError> {
        let mut entries: Vec<(u8, Prepared)> = Vec::new();
        let Some(address) = state.address else {
            return Ok(PreparedInterfaceRoutes { entries });
        };
        entries
            .try_reserve_exact(4)
            .map_err(|_| SocketError::NoMemory)?;
        let mut append = |destination: Ipv4Addr,
                          prefix_length: u8,
                          kind: Ipv4RouteKind|
         -> Result<(), SocketError> {
            let table = if kind == Ipv4RouteKind::Unicast {
                TABLE_MAIN
            } else {
                TABLE_LOCAL
            };
            let request = Ipv4RouteRequest {
                destination,
                prefix_length,
                table,
                metric: 0,
                scope: if kind == Ipv4RouteKind::Local {
                    254
                } else {
                    253
                },
                protocol: 2,
                kind,
                interface_index: Some(index),
                gateway: None,
                preferred_source: Some(address),
                on_link: false,
                metrics: Ipv4RouteMetrics::default(),
            };
            // explicit broadcast等于prefix broadcast时native FIB exact alias只保留一次。
            if entries
                .iter()
                .any(|(prefix, entry)| *prefix == prefix_length && entry.value().request == request)
            {
                return Ok(());
            }
            let order = crate::id::next_runtime_object_id();
            let record = Record {
                request,
                origin: Origin::Interface(index),
                interface_index: Some(index),
            };
            let node = FallibleMap::try_prepare(
                (table, u32::from(destination), 0, i128::from(order)),
                record,
            )
            .map_err(|_| SocketError::NoMemory)?;
            entries.push((prefix_length, node));
            Ok(())
        };
        // 1. fib_add_ifaddr在IFF_UP检查之前创建LOCAL /32；DOWN不删除scope HOST的本地成员。
        append(address, 32, Ipv4RouteKind::Local)?;
        if state.up {
            // 2. explicit与prefix broadcast可同时存在；limited broadcast不建立独立record。
            if !state.broadcast.is_unspecified() && !state.broadcast.is_broadcast() {
                append(state.broadcast, 32, Ipv4RouteKind::Broadcast)?;
            }
            let network = u32::from(address) & mask(state.prefix_length);
            if network >> 24 != 0 && (network != u32::from(address) || state.prefix_length < 32) {
                append(
                    Ipv4Addr::from(network),
                    state.prefix_length,
                    if index == 1 {
                        Ipv4RouteKind::Local
                    } else {
                        Ipv4RouteKind::Unicast
                    },
                )?;
                if state.prefix_length < 31 {
                    append(
                        Ipv4Addr::from(network | !mask(state.prefix_length)),
                        32,
                        Ipv4RouteKind::Broadcast,
                    )?;
                }
            }
        }
        Ok(PreparedInterfaceRoutes { entries })
    }
    /// 单一事务替换实际地址产生的routes；预分配后无失败，不维护第二份gateway/interface索引。
    pub(super) fn replace_interface(&mut self, index: u32, prepared: PreparedInterfaceRoutes) {
        for aliases in &mut self.prefixes {
            aliases.retain(|_, record| record.origin != Origin::Interface(index));
        }
        for (prefix, entry) in prepared.entries {
            self.prefixes[usize::from(prefix)].commit_vacant(entry);
        }
        self.reindex_prefixes();
    }
    fn reindex_prefixes(&mut self) {
        // interface事件只在冷路径，33 roots固定有界；没有全route scan重建决策或另一份route集合。
        self.used_prefixes = self
            .prefixes
            .iter()
            .enumerate()
            .fold(0, |bits, (prefix, aliases)| {
                bits | if aliases.is_empty() {
                    0
                } else {
                    1u64 << prefix
                }
            });
    }
    fn matching(
        &self,
        table: u32,
        destination: Ipv4Addr,
        bound: Option<u32>,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<&Record> {
        let mut prefixes = self.used_prefixes;
        while prefixes != 0 {
            let prefix = (63 - prefixes.leading_zeros()) as u8;
            prefixes &= !(1u64 << prefix);
            let network = u32::from(destination) & mask(prefix);
            let aliases = &self.prefixes[usize::from(prefix)];
            let mut next = aliases.ceiling(&(table, network, 0, i128::MIN));
            while let Some((&key, record)) = next {
                if key.0 != table || key.1 != network {
                    break;
                }
                let request = record.request;
                let eligible = request.scope >= minimum_scope
                    && bound.is_none_or(|bound| record.interface_index == Some(bound))
                    && record.interface_index.is_none_or(|index| {
                        interfaces
                            .clone()
                            .find(|interface| interface.index == index)
                            .is_some_and(|interface| {
                                interface.mtu >= 68
                                    && (interface.up || request.kind == Ipv4RouteKind::Local)
                            })
                    });
                if eligible {
                    return Some(record);
                }
                next = aliases.successor(&key);
            }
        }
        None
    }
    fn lookup_record(
        &self,
        destination: Ipv4Addr,
        bound: Option<u32>,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<&Record> {
        for table in [TABLE_LOCAL, TABLE_MAIN] {
            if let Some(record) =
                self.matching(table, destination, bound, minimum_scope, interfaces.clone())
                && record.request.kind != Ipv4RouteKind::Throw
            {
                return Some(record);
            }
        }
        None
    }
    /// NETDEV_DOWN/最后地址删除按native nexthop scope退休用户routes；再次UP不复活旧static配置。
    pub(super) fn device_down(&mut self, index: u32, force: bool) {
        for aliases in &mut self.prefixes {
            aliases.retain(|_, record| {
                record.interface_index != Some(index) || (!force && record.request.scope == 254)
            });
        }
        self.reindex_prefixes();
    }
    /// 原生proc/rtnetlink只读当前actual records；名字/设备状态由同一NetworkStack guard投影。
    pub(super) fn records(&self) -> impl Iterator<Item = Ipv4RouteRequest> + '_ {
        self.prefixes.iter().flat_map(|aliases| {
            aliases.iter().map(|(_, record)| Ipv4RouteRequest {
                interface_index: record.interface_index,
                ..record.request
            })
        })
    }

    /// 原生单路径配置先验证实际device/gateway/prefsrc，再准备唯一node；失败不改原FIB。
    pub(super) fn insert(
        &mut self,
        mut request: Ipv4RouteRequest,
        interfaces: &[NetworkInterface],
        replace: bool,
        create: bool,
        exclusive: bool,
        append: bool,
    ) -> Result<(), Ipv4RouteError> {
        if request.prefix_length > 32
            || request.table == 0
            || request.scope > 254
            || u32::from(request.destination) & mask(request.prefix_length)
                != u32::from(request.destination)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        // native ip_metrics_convert先规范化再比较actual alias；超大ADVMSS不是配置错误。
        request.metrics.advertised_mss = request.metrics.advertised_mss.min(65535 - 40);
        let rejecting = matches!(
            request.kind,
            Ipv4RouteKind::Unreachable | Ipv4RouteKind::Throw
        );
        let selected = if rejecting {
            if request.gateway.is_some() || request.interface_index.is_some() {
                return Err(Ipv4RouteError::Invalid);
            }
            None
        } else if let Some(gateway) = request.gateway {
            if request.scope >= 253 {
                return Err(Ipv4RouteError::Invalid);
            }
            if request.on_link {
                let index = request.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
                let interface = interfaces
                    .iter()
                    .find(|interface| interface.index == index)
                    .ok_or(Ipv4RouteError::NoDevice)?;
                if !interface.state.up {
                    return Err(Ipv4RouteError::NetworkDown);
                }
                if gateway.is_unspecified()
                    || gateway.is_multicast()
                    || gateway.is_broadcast()
                    || self
                        .lookup_record(
                            gateway,
                            None,
                            0,
                            interfaces.iter().map(InterfaceFacts::capture),
                        )
                        .is_some_and(|record| record.request.kind != Ipv4RouteKind::Unicast)
                {
                    return Err(Ipv4RouteError::Invalid);
                }
                Some(index)
            } else {
                // gateway只能经更窄scope的actual路由到达，不沿另一个default递归伪造邻居。
                let route = self
                    .lookup_record(
                        gateway,
                        request.interface_index,
                        253,
                        interfaces.iter().map(InterfaceFacts::capture),
                    )
                    .ok_or(Ipv4RouteError::NetworkUnreachable)?;
                if !matches!(
                    route.request.kind,
                    Ipv4RouteKind::Local | Ipv4RouteKind::Unicast
                ) {
                    return Err(Ipv4RouteError::Invalid);
                }
                let index = route.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
                let interface = interfaces
                    .iter()
                    .find(|interface| interface.index == index)
                    .ok_or(Ipv4RouteError::NoDevice)?;
                if !interface.state.up {
                    return Err(Ipv4RouteError::NetworkDown);
                }
                Some(index)
            }
        } else {
            if request.on_link {
                return Err(Ipv4RouteError::Invalid);
            }
            let index = request.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
            let interface = interfaces
                .iter()
                .find(|interface| interface.index == index && interface.device.ip_mtu() >= 68)
                .ok_or(Ipv4RouteError::NoDevice)?;
            if request.scope < 254 && !interface.state.up {
                return Err(Ipv4RouteError::NetworkDown);
            }
            Some(index)
        };
        if let Some(source) = request.preferred_source
            && !source.is_unspecified()
            && !self
                .lookup_record(
                    source,
                    None,
                    0,
                    interfaces.iter().map(InterfaceFacts::capture),
                )
                .is_some_and(|record| record.request.kind == Ipv4RouteKind::Local)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        let aliases = &mut self.prefixes[usize::from(request.prefix_length)];
        let network = u32::from(request.destination);
        let mut existing = aliases.ceiling(&(request.table, network, request.metric, i128::MIN));
        let mut first = None;
        let mut exact = None;
        while let Some((&key, record)) = existing {
            if (key.0, key.1, key.2) != (request.table, network, request.metric) {
                break;
            }
            first.get_or_insert(key);
            if record.request == request && record.interface_index == selected {
                exact = Some(key);
                break;
            }
            existing = aliases.successor(&key);
        }
        if exclusive && first.is_some() {
            return Err(Ipv4RouteError::Exists);
        }
        if !replace && exact.is_some() {
            return Err(Ipv4RouteError::Exists);
        }
        if replace && exact.is_some() {
            return if exact == first {
                Ok(())
            } else {
                Err(Ipv4RouteError::Exists)
            };
        }
        if !create && first.is_none() {
            return Err(Ipv4RouteError::Missing);
        }
        let record = Record {
            request,
            origin: Origin::User,
            interface_index: selected,
        };
        let order = i128::from(crate::id::next_runtime_object_id());
        let key = (
            request.table,
            network,
            request.metric,
            if append { order } else { -order },
        );
        let mut prepared =
            FallibleMap::try_prepare(key, record).map_err(|_| Ipv4RouteError::NoMemory)?;
        let retired = if replace {
            first.and_then(|key| {
                prepared.set_key(key);
                aliases.take_entry(&key)
            })
        } else {
            None
        };
        aliases.commit_vacant(prepared);
        self.used_prefixes |= 1u64 << request.prefix_length;
        // 所有值为scalar route事实，不拥有Task/NET/OFD资源；冷TaskMutex事务只销毁摘除的node。
        drop(retired);
        Ok(())
    }
    /// 原生删除匹配prefix/table及可选metric/gateway/oif；没有匹配返回ESRCH，不能清其他默认route。
    pub(super) fn remove(&mut self, request: Ipv4RouteRequest) -> Result<(), Ipv4RouteError> {
        if request.prefix_length > 32
            || u32::from(request.destination) & mask(request.prefix_length)
                != u32::from(request.destination)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        let aliases = &mut self.prefixes[usize::from(request.prefix_length)];
        let network = u32::from(request.destination);
        let mut next = aliases.ceiling(&(request.table, network, 0, i128::MIN));
        let mut selected = None;
        while let Some((&key, record)) = next {
            if key.0 != request.table || key.1 != network {
                break;
            }
            if (request.metric == 0 || request.metric == record.request.metric)
                && request
                    .interface_index
                    .is_none_or(|index| record.interface_index == Some(index))
                && request
                    .gateway
                    .is_none_or(|gateway| record.request.gateway == Some(gateway))
                && (request.kind == record.request.kind)
            {
                selected = Some(key);
                break;
            }
            next = aliases.successor(&key);
        }
        let retired = aliases.take_entry(&selected.ok_or(Ipv4RouteError::NotFound)?);
        if aliases.is_empty() {
            self.used_prefixes &= !(1u64 << request.prefix_length);
        }
        drop(retired);
        Ok(())
    }

    /// 原生最长前缀/metric与LOCAL→lo选择；oif/direct fallback由同一个flow入口处理。
    pub(super) fn select(
        &self,
        destination: Ipv4Addr,
        bound: Option<u32>,
        interfaces: &[NetworkInterface],
    ) -> Result<RouteSelection, Ipv4RouteKind> {
        self.select_facts(
            destination,
            bound,
            interfaces.iter().map(InterfaceFacts::capture),
        )
    }
    pub(super) fn select_facts(
        &self,
        destination: Ipv4Addr,
        bound: Option<u32>,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, Ipv4RouteKind> {
        // limited broadcast/本地组播的明确oif绕过FIB gateway；原生route_output先处理。
        let local_multicast = destination.octets()[..3] == [224, 0, 0];
        if bound.is_some() && (destination.is_broadcast() || local_multicast) {
            return Self::direct(destination, bound, interfaces);
        }
        if let Some(record) = self.lookup_record(destination, bound, 0, interfaces.clone()) {
            let kind = record.request.kind;
            if matches!(kind, Ipv4RouteKind::Unreachable) {
                // 原生explicit oif在FIB失败时使用on-link fallback，不能让kernel另写selector。
                return if bound.is_some() {
                    Self::direct(destination, bound, interfaces)
                } else {
                    Err(kind)
                };
            }
            let logical = record
                .interface_index
                .expect("forwarding route must own actual device");
            let output = if kind == Ipv4RouteKind::Local {
                1
            } else {
                logical
            };
            let Some(output_interface) = interfaces
                .clone()
                .find(|interface| interface.index == output && interface.up)
            else {
                return Err(Ipv4RouteKind::Throw);
            };
            return Ok(RouteSelection {
                interface_index: output,
                logical_interface_index: logical,
                preferred_source: record.request.preferred_source.or_else(|| {
                    if kind == Ipv4RouteKind::Local {
                        Some(destination)
                    } else {
                        interfaces
                            .clone()
                            .find(|interface| interface.index == logical)
                            .and_then(|interface| interface.address)
                    }
                }),
                next_hop: if destination.is_broadcast() || destination.is_multicast() {
                    destination
                } else {
                    record.request.gateway.unwrap_or(destination)
                },
                kind: if destination.is_broadcast() {
                    Ipv4RouteKind::Broadcast
                } else {
                    kind
                },
                transport: record.request.metrics.transport(output_interface.mtu),
            });
        }
        // 没有matching FIB只允许实际明确oif的on-link发送；无oif不伪造default。
        Self::direct(destination, bound, interfaces)
    }
    fn direct(
        destination: Ipv4Addr,
        bound: Option<u32>,
        mut interfaces: impl Iterator<Item = InterfaceFacts>,
    ) -> Result<RouteSelection, Ipv4RouteKind> {
        if let Some(index) = bound
            && let Some(interface) = interfaces
                .find(|interface| interface.index == index && interface.up && interface.mtu >= 68)
        {
            return Ok(RouteSelection {
                interface_index: index,
                logical_interface_index: index,
                preferred_source: interface.address,
                next_hop: destination,
                kind: if destination.is_broadcast() {
                    Ipv4RouteKind::Broadcast
                } else {
                    Ipv4RouteKind::Unicast
                },
                transport: Ipv4RouteMetrics::default().transport(interface.mtu),
            });
        }
        Err(Ipv4RouteKind::Throw)
    }
    /// 实际LOCAL成员用于bind与prefsrc；DOWN保留HOST record，不伪造127/8成员。
    pub(super) fn is_local(&self, address: Ipv4Addr, interfaces: &[NetworkInterface]) -> bool {
        self.lookup_record(
            address,
            None,
            0,
            interfaces.iter().map(InterfaceFacts::capture),
        )
        .is_some_and(|record| record.request.kind == Ipv4RouteKind::Local)
    }
    /// ingress目的分类与broadcast/multicast spec_dst使用同一FIB；结果在RX入队冻结。
    pub(super) fn ingress(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        index: u32,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> smoltcp::iface::Ipv4IngressFacts {
        let kind = self
            .lookup_record(destination, None, 0, interfaces.clone())
            .map(|record| record.request.kind);
        let broadcast = destination.is_broadcast() || kind == Some(Ipv4RouteKind::Broadcast);
        let local = kind == Some(Ipv4RouteKind::Local);
        // 一次reverse查找同时投影spec_dst/MTU；同设备bound只收紧同一匹配，
        // 在同guard无mutation下可直接消费这个结论，不重复扫描相同FIB。
        let reverse = self.select_facts(source, None, interfaces.clone()).ok();
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
                .select_facts(source, Some(index), interfaces.clone())
                .ok(),
        };
        let reverse_route = reverse.map(|route| route.transport);
        let bound_reverse_route = bound_reverse.map(|route| route.transport);
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
}
