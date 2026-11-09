//! 一个域的实际IPv4 FIB；所有前缀/metric/自动与用户路由共享同一记录和查找。
use super::{InterfaceState, interface::NetworkInterface};
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use network_protocol::{
    Ipv4RouteError, Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest, SocketError,
};

mod admission;
mod configuration;
mod fib;
mod forwarding;
use fib::{Draft, Entry, Fib};
mod group;
mod nexthop;
pub(super) use configuration::PreparedRouteChange;
use prefix_index as poptrie;

pub(super) const TABLE_MAIN: u32 = 254;
pub(super) const TABLE_LOCAL: u32 = 255;

/// 路由的传输参数：未配置的 advertised MSS 按出口 MTU 推导。
fn transport(metrics: Ipv4RouteMetrics, mtu: usize) -> smoltcp::iface::IpRouteContext {
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
    pub(super) next_hop: Ipv4Addr,
    pub(super) kind: Ipv4RouteKind,
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
impl RoutingTable {
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
        let candidates = self.fib.matching(destination);
        for table in [TABLE_LOCAL, TABLE_MAIN] {
            if let Some(record) = self.matching(
                table,
                Flow {
                    source,
                    destination,
                    bound,
                },
                candidates.clone(),
                minimum_scope,
                interfaces.clone(),
                &include,
            ) && record.request.kind != Ipv4RouteKind::Throw
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
    ) -> Result<RouteSelection, Ipv4RouteKind> {
        // limited broadcast/本地组播的明确oif绕过FIB gateway；原生route_output先处理。
        let local_multicast = destination.octets()[..3] == [224, 0, 0];
        if bound.is_some() && (destination.is_broadcast() || local_multicast) {
            return Self::direct(destination, bound, interfaces);
        }
        if let Some(record) =
            self.lookup_record(source, destination, bound, 0, interfaces.clone(), |_| true)
        {
            let kind = record.request.kind;
            if matches!(
                kind,
                Ipv4RouteKind::Unreachable | Ipv4RouteKind::Blackhole | Ipv4RouteKind::Prohibit
            ) {
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
                transport: transport(Ipv4RouteMetrics::default(), interface.mtu),
            });
        }
        Err(Ipv4RouteKind::Throw)
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
        .is_some_and(|record| record.request.kind == Ipv4RouteKind::Local)
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
    /// ingress目的分类与broadcast/multicast spec_dst使用同一FIB；结果在RX入队冻结。
    pub(super) fn ingress(
        &self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        index: u32,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> smoltcp::iface::Ipv4IngressFacts {
        let kind = self
            .lookup_record(source, destination, None, 0, interfaces.clone(), |_| true)
            .map(|record| record.request.kind);
        let broadcast = destination.is_broadcast() || kind == Some(Ipv4RouteKind::Broadcast);
        let local = kind == Some(Ipv4RouteKind::Local);
        // 一次reverse查找同时投影spec_dst/MTU；同设备bound只收紧同一匹配，
        // 在同guard无mutation下可直接消费这个结论，不重复扫描相同FIB。
        let reverse = self
            .select_facts(destination, source, None, interfaces.clone())
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
                .select_facts(destination, source, Some(index), interfaces.clone())
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
