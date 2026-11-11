//! 接口上的地址集合与共享 solicited-node 组；各地址仍使用同一 DAD 状态机。
use super::*;

// 每类一次至多四个实际控制提交；失败等设备活动，预算耗尽才续调度，不把背压变为忙循环。
struct ControlBudget<'a, F> {
    send: &'a mut F,
    remaining: u8,
    blocked: bool,
    deferred: bool,
    sent: u8,
}
impl<'a, F: FnMut(Packet<'_>) -> bool> ControlBudget<'a, F> {
    fn new(send: &'a mut F) -> Self {
        Self {
            send,
            remaining: 4,
            blocked: false,
            deferred: false,
            sent: 0,
        }
    }
    fn submit(&mut self, packet: Packet<'_>) -> bool {
        if self.blocked {
            return false;
        }
        if self.remaining == 0 {
            self.deferred = true;
            return false;
        }
        self.remaining -= 1;
        if (self.send)(packet) {
            self.sent += 1;
            true
        } else {
            self.blocked = true;
            false
        }
    }
    fn pending(&self) -> bool {
        self.deferred && !self.blocked
    }
}
impl InterfaceAddresses {
    /// 准备共享组并取得一个 socket 引用；OOM/溢出不改变可见资格或 MLD 时钟。
    pub(in crate::inet) fn add_group(
        &mut self,
        group: Ipv6Addr,
        now: u64,
    ) -> Result<(), network_protocol::SocketError> {
        if let Some(listener) = self
            .listener
            .iter_mut()
            .chain(self.groups.iter_mut())
            .find(|listener| listener.group() == group)
        {
            listener.users = listener
                .users
                .checked_add(1)
                .ok_or(network_protocol::SocketError::NoMemory)?;
            self.synchronize_groups(now);
            return Ok(());
        }
        self.groups
            .try_reserve(1)
            .map_err(|_| network_protocol::SocketError::NoMemory)?;
        let mut listener = Listener::new(group);
        listener.users = 1;
        listener.synchronize(true, now, now, self.mld.robustness());
        let at = self
            .groups
            .binary_search_by_key(&group, Listener::group)
            .unwrap_err();
        self.groups.insert(at, listener);
        Ok(())
    }
    /// 释放 socket 引用；保留地址必需组和最后一次退组的重传义务，无分配。
    pub(in crate::inet) fn drop_group(&mut self, group: Ipv6Addr, now: u64) {
        if let Some(listener) = self
            .listener
            .iter_mut()
            .chain(self.groups.iter_mut())
            .find(|listener| listener.group() == group)
        {
            listener.users = listener
                .users
                .checked_sub(1)
                .expect("socket owns one MLD reference");
            self.synchronize_groups(now);
            self.retire_inactive_groups();
        }
    }
    /// 退休零引用且无退组义务的记录；保留容器容量供预备配置的无分配激活/回滚使用。
    pub(super) fn retire_inactive_groups(&mut self) {
        self.groups.retain(|listener| {
            !listener.inactive()
                || listener.users != 0
                || self
                    .addresses
                    .iter()
                    .any(|address| solicited(address.address) == listener.group())
        });
    }
    pub(super) fn all_addresses(&self) -> impl Iterator<Item = &Address> + Clone {
        core::iter::once(&self.local)
            .chain(self.addresses.iter())
            .chain(self.discovery.addresses.iter().map(|row| &row.address))
    }
    /// 借用完整地址意图，FIB 与查询读取同一 owner；不复制资格或额外持有配置状态。
    pub(in crate::inet) fn definitions<'a>(
        &'a self,
        replacement: Option<&'a PreparedAddresses>,
    ) -> impl Iterator<Item = router_protocol::configuration::Ipv6Address> + Clone + 'a {
        let addresses = replacement.map_or(self.addresses.as_slice(), |prepared| {
            prepared.addresses.as_slice()
        });
        core::iter::once(self.local.definition()).chain(addresses.iter().map(Address::definition))
    }
    pub(in crate::inet) fn has_address(&self, address: Ipv6Addr) -> bool {
        self.all_addresses()
            .any(|value| value.address == address && value.usable())
    }
    /// 冻结所需的单行值；地址资格与前缀仍由原对象提供，未启动的地址不伪造为可用。
    pub(in crate::inet) fn rows(
        &self,
        index: u32,
        name: [u8; 16],
        interface_ready: bool,
        now: u64,
    ) -> impl Iterator<Item = network_protocol::InterfaceAddress> + Clone {
        use network_protocol::{AddressOrigin, AddressState};
        let infinite = (u32::MAX, u32::MAX);
        core::iter::once((&self.local, AddressOrigin::Interface, infinite))
            .chain(
                self.addresses
                    .iter()
                    .map(move |address| (address, AddressOrigin::Configured, infinite)),
            )
            .chain(
                self.discovery
                    .addresses
                    .iter()
                    .map(move |row| (&row.address, AddressOrigin::Slaac, row.remaining(now))),
            )
            .filter_map(
                move |(address, origin, (preferred_lifetime, valid_lifetime))| {
                    if valid_lifetime == 0 {
                        return None;
                    }
                    // DAD 仍由接口推进。即使下一次 poll 尚未执行，已到期的优选资格也不出现在快照中。
                    let state = match address.state() {
                        Some(AddressState::Preferred) if preferred_lifetime == 0 => {
                            AddressState::Deprecated
                        }
                        Some(state) => state,
                        None if origin == AddressOrigin::Configured => AddressState::Tentative,
                        None => return None,
                    };
                    Some(network_protocol::InterfaceAddress {
                        interface_index: index,
                        name,
                        address: address.address.into(),
                        prefix_length: address.prefix_length,
                        state,
                        usable: interface_ready && address.usable(),
                        origin,
                        on_link: address.on_link,
                        broadcast: None,
                        preferred_lifetime,
                        valid_lifetime,
                    })
                },
            )
    }
    pub(super) fn preferred_link(&self) -> Option<Ipv6Addr> {
        self.all_addresses()
            .find(|address| address.preferred() && address.address.is_unicast_link_local())
            .map(|address| address.address)
    }
    pub(in crate::inet) fn control_source(&self) -> Option<Ipv6Addr> {
        self.preferred_link().or_else(|| {
            self.all_addresses()
                .find(|address| address.usable())
                .map(|address| address.address)
        })
    }
    pub(in crate::inet) fn source_for(&self, destination: Ipv6Addr) -> Option<Ipv6Addr> {
        super::selection::source(self.all_addresses(), destination)
    }
    pub(super) fn joined(
        listener: &Option<Listener>,
        groups: &[Listener],
        address: Ipv6Addr,
    ) -> bool {
        let group = solicited(address);
        listener
            .as_ref()
            .filter(|listener| listener.group() == group)
            .or_else(|| {
                groups
                    .binary_search_by_key(&group, Listener::group)
                    .ok()
                    .map(|index| &groups[index])
            })
            .is_some_and(Listener::ready)
    }
    pub(super) fn synchronize_groups(&mut self, now: u64) {
        let local = &self.local;
        let addresses = &self.addresses;
        let learned = &self.discovery.addresses;
        for listener in self.listener.iter_mut().chain(self.groups.iter_mut()) {
            let mut start = None;
            for address in core::iter::once(local)
                .chain(addresses.iter())
                .chain(learned.iter().map(|row| &row.address))
            {
                if matches!(
                    address.state(),
                    Some(
                        AddressState::Tentative
                            | AddressState::Preferred
                            | AddressState::Deprecated
                    )
                ) && solicited(address.address) == listener.group()
                {
                    let at = address.deadline(true).unwrap_or(now);
                    start = Some(start.map_or(at, |old: u64| old.min(at)));
                }
            }
            if listener.users != 0 {
                start = Some(start.map_or(now, |at| at.min(now)));
            }
            listener.synchronize(
                start.is_some(),
                start.unwrap_or(now),
                now,
                self.mld.robustness(),
            );
        }
    }
    pub(in crate::inet) fn qualify_loopback(&mut self, enabled: bool) -> bool {
        for listener in &mut self.groups {
            listener.quiet();
        }
        self.groups.retain(|listener| listener.users != 0);
        let mut changed = false;
        for address in core::iter::once(&mut self.local).chain(self.addresses.iter_mut()) {
            let previous = address.state();
            if enabled {
                address.enable_loopback();
            } else {
                address.disable();
            }
            changed |= previous != address.state();
        }
        changed
    }
    pub(super) fn advance_addresses(
        &mut self,
        enabled: bool,
        accept_ra: bool,
        now: u64,
        mtu: usize,
        mut send: impl FnMut(Packet<'_>) -> bool,
        mut dropped: impl FnMut(),
    ) -> (bool, bool) {
        if self.listener.is_none() {
            return (self.qualify_loopback(enabled && mtu >= 1280), false);
        }
        if !enabled || mtu < 1280 {
            let changed = self
                .all_addresses()
                .any(|address| address.state().is_some());
            for _ in 0..self.disable() {
                dropped();
            }
            return (changed, false);
        }
        let mut changed = self.discovery.expire(now);
        for address in core::iter::once(&mut self.local).chain(self.addresses.iter_mut()) {
            changed |= address.start(now).is_some();
        }
        self.synchronize_groups(now);
        if self.mld.expire(now) {
            for listener in self.listener.iter_mut().chain(self.groups.iter_mut()) {
                if listener.ready() {
                    listener.address_ready(now);
                }
            }
        }
        let source = self.preferred_link().unwrap_or(Ipv6Addr::UNSPECIFIED);
        let count = self.groups.len() + 1;
        let mut budget = ControlBudget::new(&mut send);
        let start = self.group_cursor % count;
        for step in 0..count {
            let index = (start + step) % count;
            let listener = if index == 0 {
                self.listener.as_mut().expect("Ethernet listener")
            } else {
                &mut self.groups[index - 1]
            };
            let before = budget.sent;
            if listener.group() == super::packet::ALL_NODES || listener.group().octets()[1] & 15 < 2
            {
                listener.quiet();
                continue;
            }
            use crate::inet::multicast::listener::{Mode, Report};
            let mode = if self.mld.v1(now) {
                Mode::Legacy { leave: true }
            } else {
                Mode::Current
            };
            listener.poll(now, mtu.saturating_sub(76) / 16, mode, &mut |report| {
                budget.submit(match report {
                    Report::Legacy { group, leave } => Packet::ReportV1 {
                        source,
                        group,
                        done: leave,
                    },
                    Report::Current {
                        group,
                        kind,
                        sources,
                    } => Packet::ReportV2 {
                        source,
                        group,
                        kind,
                        sources,
                    },
                })
            });
            if budget.sent != before {
                self.group_cursor = (index + 1) % count;
            }
        }
        let mut pending = budget.pending();
        let retransmit_ms = self.neighbors.retransmit_ms();
        let count = self.addresses.len() + self.discovery.addresses.len() + 1;
        let start = self.address_cursor % count;
        let mut budget = ControlBudget::new(&mut send);
        for step in 0..count {
            let index = (start + step) % count;
            let address = if index == 0 {
                &mut self.local
            } else if index <= self.addresses.len() {
                &mut self.addresses[index - 1]
            } else {
                &mut self.discovery.addresses[index - 1 - self.addresses.len()].address
            };
            let joined = Self::joined(&self.listener, &self.groups, address.address);
            let before = budget.sent;
            address.poll(now, joined, retransmit_ms, |packet| budget.submit(packet));
            if budget.sent != before {
                self.address_cursor = (index + 1) % count;
            }
        }
        pending |= budget.pending();
        let source = self.preferred_link();
        self.discovery
            .poll_solicit(accept_ra, source, now, &mut send);
        self.retire_inactive_groups();
        (changed, pending)
    }
    pub(super) fn confirm_addresses(&mut self, now: u64) -> bool {
        let previous = self.preferred_link();
        let mut changed = false;
        for address in core::iter::once(&mut self.local)
            .chain(self.addresses.iter_mut())
            .chain(
                self.discovery
                    .addresses
                    .iter_mut()
                    .map(|row| &mut row.address),
            )
        {
            changed |= address.confirm(
                now,
                Self::joined(&self.listener, &self.groups, address.address),
            );
        }
        if previous != self.preferred_link() {
            for listener in self.listener.iter_mut().chain(self.groups.iter_mut()) {
                listener.address_ready(now);
            }
        }
        changed | self.discovery.refresh_preference(now)
    }
}
