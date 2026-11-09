//! 配置事务的连续 FIB 快照：预备候选根，发布/撤销交换所有权，退出的根在锁外退休。

use super::*;
use crate::inet::configuration::InterfaceChange;

/// FIB 根或单个下一跳的唯一前像；撤销按对应粒度交换，退出的根在 NET guard 外退休。
pub(in crate::inet) struct PreparedRouteChange {
    change: Change,
    // OWNER: 预备时的设备身份集合。配置租约排斥状态写入，身份不复用；
    // 若没有设备退休就可直接交换根，避免每次发布扫描百万条路由。
    known_interfaces: Vec<u32>,
}

enum Change {
    Root(Fib),
    NextHop(nexthop::Change),
}
impl PreparedRouteChange {
    fn new(change: Change, interfaces: &[NetworkInterface]) -> Result<Self, SocketError> {
        let mut known_interfaces = Vec::new();
        known_interfaces
            .try_reserve_exact(interfaces.len())
            .map_err(|_| SocketError::NoMemory)?;
        known_interfaces.extend(interfaces.iter().map(|interface| interface.index));
        Ok(Self {
            change,
            known_interfaces,
        })
    }
}

pub(super) fn configuration_facts<'a>(
    interfaces: &'a [NetworkInterface],
    changes: &'a [InterfaceChange],
) -> impl Clone + Iterator<Item = InterfaceFacts> + 'a {
    interfaces.iter().map(|interface| {
        let change = changes
            .iter()
            .find(|change| change.plan.index == interface.index);
        let state = change.map_or(interface.state, |change| change.next);
        InterfaceFacts {
            index: interface.index,
            address: state.address,
            up: state.up,
            mtu: change.map_or_else(|| interface.device.ip_mtu(), InterfaceChange::next_mtu),
        }
    })
}
pub(super) fn replaced(record: &Record, changes: &[InterfaceChange], fib: &Fib) -> bool {
    changes.iter().any(|change| {
        let index = change.plan.index;
        record.origin == Origin::Interface(index)
            || !fib.is_multipath(record.next_hop_name)
                && record.interface_index == Some(index)
                && (change.next_mtu() < 68
                    || change.previous.address != change.next.address
                    || change.previous.up && !change.next.up && record.request.scope != 254)
    })
}

impl RoutingTable {
    /// 在原 FIB 未改动时准备整个批次；完整候选根与设备身份在此准备。
    pub(in crate::inet) fn prepare_configuration(
        &self,
        changes: &[InterfaceChange],
        interfaces: &[NetworkInterface],
    ) -> Result<PreparedRouteChange, SocketError> {
        let adjacency = Self::configuration_draft(changes)?;
        let mut candidate = Draft::new(0)?;
        candidate.inherit_named(&self.fib)?;
        for change in changes {
            if change.next_mtu() < 68 || change.previous.address != change.next.address {
                candidate.retire_interface(change.plan.index, true);
            } else if change.previous.up && !change.next.up {
                candidate.retire_interface(change.plan.index, false);
            }
        }
        candidate.extend(adjacency)?;
        self.prepare_change(
            candidate,
            |record| replaced(record, changes, &self.fib),
            interfaces,
            configuration_facts(interfaces, changes),
        )
    }

    /// 小规模自动接口邻接候选不含受管前缀或默认路由意图；网关准入只在执行 FIB 中进行。
    pub(super) fn configuration_draft(changes: &[InterfaceChange]) -> Result<Draft, SocketError> {
        let mut candidate = Draft::new(changes.len().checked_mul(5).ok_or(SocketError::NoMemory)?)?;
        for change in changes {
            candidate.extend(Self::interface_entries(change.plan.index, change.next)?)?;
        }
        Ok(candidate)
    }

    pub(super) fn prepare_change(
        &self,
        candidate: Draft,
        replace: impl Fn(&Record) -> bool,
        interfaces: &[NetworkInterface],
        facts: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Result<PreparedRouteChange, SocketError> {
        let fib = self.fib.prepare(candidate, replace)?;
        let mut candidate = Self { fib };
        candidate.validate_sources(facts)?;
        PreparedRouteChange::new(Change::Root(candidate.fib), interfaces)
    }

    pub(in crate::inet) fn prepare_next_hop(
        &self,
        definition: network_protocol::routes::NextHopConfiguration<'_>,
        interfaces: &[NetworkInterface],
    ) -> Result<PreparedRouteChange, SocketError> {
        let name = nexthop::Name::new(definition.id).ok_or(SocketError::Invalid)?;
        let hop = self.admit_next_hop(definition, interfaces)?;
        let change = self.fib.prepare_next_hop(name, hop)?;
        PreparedRouteChange::new(Change::NextHop(change), interfaces)
    }

    /// 用同一候选接口事实合并未受影响的活动前缀与小规模新邻接；不读取已撤销来源或扫描新受管全表。
    pub(super) fn configuration_lookup(
        &self,
        flow: Flow,
        minimum_scope: u8,
        candidate: &Draft,
        changes: &[InterfaceChange],
        interfaces: &[NetworkInterface],
        replace_forwarding: bool,
    ) -> Option<Record> {
        let facts = configuration_facts(interfaces, changes);
        let candidates = self.fib.matching(flow.destination);
        for table in [TABLE_LOCAL, TABLE_MAIN] {
            let old = self.matching(
                table,
                flow,
                candidates.clone(),
                minimum_scope,
                facts.clone(),
                |record| {
                    (!replace_forwarding || record.origin != Origin::Forwarding)
                        && !replaced(record, changes, &self.fib)
                        && !changes.iter().any(|change| {
                            record.interface_index == Some(change.plan.index)
                                && (change.next_mtu() < 68
                                    || change.previous.address != change.next.address
                                    || change.previous.up
                                        && !change.next.up
                                        && record.request.scope != 254)
                        })
                },
            );
            let new = candidate.lookup_record(table, flow, minimum_scope, facts.clone());
            let selected = match (old, new) {
                (Some(old), Some(new)) => Some(
                    if (
                        new.request.prefix_length,
                        core::cmp::Reverse(new.request.metric),
                    ) > (
                        old.request.prefix_length,
                        core::cmp::Reverse(old.request.metric),
                    ) {
                        new
                    } else {
                        old
                    },
                ),
                (old, new) => old.or(new),
            };
            if let Some(record) = selected
                && record.request.kind != Ipv4RouteKind::Throw
            {
                return Some(record);
            }
        }
        None
    }

    /// 没有预备后热拔出时只交换根；临界区工作与路由条数无关。
    pub(in crate::inet) fn activate_change(
        &mut self,
        prepared: &mut PreparedRouteChange,
        interfaces: &[NetworkInterface],
    ) {
        match &mut prepared.change {
            Change::Root(fib) => {
                if prepared
                    .known_interfaces
                    .iter()
                    .any(|index| !interfaces.iter().any(|interface| interface.index == *index))
                {
                    fib.remove_missing(interfaces);
                }
                core::mem::swap(&mut self.fib, fib);
            }
            Change::NextHop(change) => {
                self.fib.exchange_next_hop(change);
            }
        }
    }

    /// 恢复原根；热拔出的身份必须在可见前移除，不能因撤销复活设备。无分配。
    pub(in crate::inet) fn rollback_change(
        &mut self,
        prepared: &mut PreparedRouteChange,
        interfaces: &[NetworkInterface],
    ) -> Result<(), SocketError> {
        let restored = match &mut prepared.change {
            Change::Root(fib) => {
                core::mem::swap(&mut self.fib, fib);
                None
            }
            Change::NextHop(change) => Some(
                self.fib
                    .exchange_next_hop(change)
                    .members()
                    .iter()
                    .any(|member| {
                        !interfaces
                            .iter()
                            .any(|interface| interface.index == member.interface_index)
                    }),
            ),
        };
        if prepared
            .known_interfaces
            .iter()
            .any(|index| !interfaces.iter().any(|interface| interface.index == *index))
        {
            self.fib.remove_missing(interfaces);
        }
        if restored == Some(true) {
            return Err(SocketError::NoDevice);
        }
        Ok(())
    }
}
