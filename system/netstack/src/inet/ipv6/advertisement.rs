//! RFC 4861 下游静态通告。接口拥有一个发送时钟，收到的请求共用下一次组播。
use super::{
    InterfaceAddresses, NdMessage, NdPacket, Packet, SendTime, delegation_valid,
    packet::ALL_ROUTERS, random_delay,
};
use alloc::vec::Vec;
use core::net::Ipv6Addr;
use network_protocol::SocketError;
use router_protocol::configuration::{
    ADVERTISEMENT_OPTION_SPACE, Ipv6Address, MAX_ADVERTISED_DNS, MAX_ADVERTISED_PREFIXES,
    RouterAdvertisements, rdnss_size,
};
use router_protocol::delegation::DelegatedPrefix;
use router_protocol::interface::{DelegatedPrefixes, Ipv6Addresses};

const MIN_MULTICAST_INTERVAL: u64 = 3000;
const MAX_INITIAL_INTERVAL: u64 = 16_000;
const MAX_INITIAL_ADVERTISEMENTS: u8 = 3;
const MAX_RESPONSE_DELAY: u64 = 500;
// 三帧对应 RFC 4861 的最大停用通告次数；整个历史必须可分批撤回。
const MAX_PREFIX_HISTORY: usize = 3 * MAX_ADVERTISED_PREFIXES;
const PREFERRED_LIFETIME: u32 = 2700;
const VALID_LIFETIME: u32 = 5400;

fn remaining(deadline: i64, now: u64, limit: u32) -> u32 {
    if deadline == i64::MAX {
        return limit;
    }
    let seconds = (deadline as u64 / 1_000_000).saturating_sub(now) / 1000;
    seconds.min(u64::from(limit)) as u32
}

pub(super) type PrefixInformation = (Ipv6Addr, u32, u32);

struct AnnouncedPrefix {
    prefix: Ipv6Addr,
    expires: u64,
    stale: bool,
}

#[derive(Default)]
pub(super) struct Advertiser {
    policy: RouterAdvertisements,
    // 此引用与应用的 ff02::2 订阅共用 MLD listener。只有获得实际引用后才开始广告。
    joined: bool,
    next: Option<SendTime>,
    // 最近一次组播及是否还需要在停用时发送撤回；撤回本身也约束下一次组播间距。
    last: Option<(u64, bool)>,
    initial: u8,
    // 只记录已成功提交的正寿命 PIO，作为旧配置和未确认事务均无法替代的撤回依据。
    prefixes: Vec<AnnouncedPrefix>,
    // Only the last successfully submitted set is needed. Every positive RDNSS set
    // includes all live servers and every removed address is withdrawn in the same packet.
    dns: Vec<Ipv6Addr>,
    dns_expires: u64,
    // 本轮首帧已经提交后，从这个前缀之后继续撤回；:: 表示首帧没有空间放旧前缀。
    withdraw_after: Option<Ipv6Addr>,
}

impl Advertiser {
    fn earliest(&self) -> u64 {
        self.last
            .map_or(0, |(last, _)| last.saturating_add(MIN_MULTICAST_INTERVAL))
    }

    fn needs_withdrawal(&self) -> bool {
        self.last.is_some_and(|(_, announced)| announced)
            || self.withdraw_after.is_some()
            || !self.dns.is_empty()
    }

    fn record_dns(&mut self, live: &[Ipv6Addr], now: u64, lifetime: u32) {
        assert!(self.dns.capacity() >= live.len(), "RDNSS history prepared");
        self.dns.clear();
        self.dns.extend_from_slice(live);
        self.dns_expires = now.saturating_add(u64::from(lifetime) * 1000);
    }

    pub(super) fn next_deadline(
        &self,
        now: u64,
        source_ready: impl FnOnce() -> bool,
    ) -> Option<u64> {
        if self.joined || self.needs_withdrawal() {
            if !source_ready() {
                return None;
            }
            self.next.map_or(Some(now), SendTime::deadline)
        } else {
            self.next.and_then(SendTime::deadline)
        }
    }
}

impl InterfaceAddresses {
    /// 在发布前准备通告历史容量。新旧配置及已宣布前缀必须可共同保留，激活和回滚不分配。
    pub(in crate::inet) fn prepare_advertised_prefixes(
        &mut self,
        replacement: Option<Ipv6Addresses<'_>>,
        delegated: Option<DelegatedPrefixes<'_>>,
        policy: RouterAdvertisements,
        now: u64,
    ) -> Result<(), SocketError> {
        let mut prefixes = [Ipv6Addr::UNSPECIFIED; MAX_PREFIX_HISTORY];
        let mut count = 0;
        let prospective = delegated
            .into_iter()
            .flat_map(DelegatedPrefixes::iter)
            .filter(|prefix| delegation_valid(prefix, now))
            .map(|prefix| prefix.address);
        let mut live = [Ipv6Addr::UNSPECIFIED; MAX_ADVERTISED_PREFIXES];
        let mut live_count = 0;
        for prefix in self
            .definitions(None)
            .filter(|_| replacement.is_none())
            .chain(replacement.into_iter().flat_map(Ipv6Addresses::iter))
            .filter_map(Ipv6Address::advertised_prefix)
            .chain(prospective)
            .chain(
                self.delegated
                    .iter()
                    .filter(|prefix| delegated.is_none() && delegation_valid(prefix, now))
                    .map(|prefix| prefix.address),
            )
        {
            if !live[..live_count].contains(&prefix) {
                *live.get_mut(live_count).ok_or(SocketError::Invalid)? = prefix;
                live_count += 1;
            }
        }
        if live_count > policy.max_prefixes() {
            return Err(SocketError::Invalid);
        }
        for prefix in self
            .advertiser
            .prefixes
            .iter()
            .filter(|entry| entry.expires > now)
            .map(|entry| entry.prefix)
            .chain(
                self.definitions(None)
                    .filter(|_| {
                        replacement.is_none()
                            || self.advertiser.policy != RouterAdvertisements::Disabled
                    })
                    .filter_map(Ipv6Address::advertised_prefix),
            )
            .chain(
                self.delegated
                    .iter()
                    .filter(|prefix| delegation_valid(prefix, now))
                    .map(|prefix| prefix.address),
            )
            .chain(live[..live_count].iter().copied())
        {
            if !prefixes[..count].contains(&prefix) {
                *prefixes.get_mut(count).ok_or(SocketError::NoBuffer)? = prefix;
                count += 1;
            }
        }
        self.advertiser
            .prefixes
            .try_reserve_exact(count.saturating_sub(self.advertiser.prefixes.len()))
            .map_err(|_| SocketError::NoMemory)
    }

    /// 预留候选 DNS 列表的发布历史。已有容量继续供原策略或回滚使用。
    /// # Errors
    /// 无法为候选服务器保留空间。
    pub(in crate::inet) fn prepare_advertised_dns(
        &mut self,
        policy: RouterAdvertisements,
    ) -> Result<(), SocketError> {
        let count = policy.dns().as_slice().len();
        self.advertiser
            .dns
            .try_reserve_exact(count.saturating_sub(self.advertiser.dns.len()))
            .map_err(|_| SocketError::NoMemory)
    }

    /// 预备下游租约的执行记录，原值留在接口直到整批发布。
    pub(in crate::inet) fn prepare_delegated(
        &self,
        intent: DelegatedPrefixes<'_>,
        now: u64,
    ) -> Result<Option<Vec<DelegatedPrefix>>, SocketError> {
        let live = intent.iter().filter(|prefix| delegation_valid(prefix, now));
        if self.delegated.iter().copied().eq(live.clone()) {
            return Ok(None);
        }
        let mut next = Vec::new();
        next.try_reserve_exact(intent.len())
            .map_err(|_| SocketError::NoMemory)?;
        next.extend(live);
        Ok(Some(next))
    }

    /// 激活和撤销都用交换，逆向记录保留原截止时间。
    pub(in crate::inet) fn exchange_delegated(&mut self, prefixes: &mut Vec<DelegatedPrefix>) {
        core::mem::swap(&mut self.delegated, prefixes);
        self.reschedule_advertisements();
    }

    /// 当前下游通告配置，供执行事务验证与交换。
    pub(in crate::inet) fn advertisement_policy(&self) -> RouterAdvertisements {
        self.advertiser.policy
    }

    /// 交换配置，保留上次实际提交时刻及组播间距约束。
    pub(in crate::inet) fn exchange_advertisements(&mut self, policy: &mut RouterAdvertisements) {
        core::mem::swap(&mut self.advertiser.policy, policy);
        self.reschedule_advertisements();
    }

    pub(super) fn reschedule_advertisements(&mut self) {
        self.advertiser.initial = 0;
        self.advertiser.next = None;
        self.advertiser.withdraw_after = None;
    }

    pub(super) fn stop_advertisements(&mut self) {
        if self.advertiser.joined {
            self.drop_group(ALL_ROUTERS, 0);
        }
        self.advertiser.joined = false;
        self.advertiser.next = None;
        self.advertiser.last = None;
        self.advertiser.initial = 0;
        self.advertiser.withdraw_after = None;
    }

    pub(super) fn reconcile_advertisement_group(&mut self, router: bool, now: u64) {
        let wanted = router && self.advertiser.policy != RouterAdvertisements::Disabled;
        if wanted && !self.advertiser.joined {
            if self.advertiser.next.is_some_and(|next| !next.ready(now)) {
                return;
            }
            if self.add_group(ALL_ROUTERS, now).is_err() {
                self.advertiser.next = Some(SendTime::At(now.saturating_add(1000)));
                return;
            }
            self.advertiser.joined = true;
            self.advertiser.next = None;
        } else if !wanted && self.advertiser.joined {
            self.drop_group(ALL_ROUTERS, now);
            self.advertiser.joined = false;
            self.advertiser.next = None;
            self.advertiser.withdraw_after = None;
        } else if !wanted && !self.advertiser.needs_withdrawal() {
            self.advertiser.next = None;
        }
    }

    pub(super) fn receive_solicitation(
        &mut self,
        packet: NdPacket<'_>,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        now: u64,
    ) {
        if !self.advertiser.joined
            || self.advertiser.policy == RouterAdvertisements::Disabled
            || packet.message() != NdMessage::RouterSolicitation
            || destination != ALL_ROUTERS && !self.has_address(destination)
            || self.all_addresses().any(|local| local.address == source)
        {
            return;
        }
        let mut link_address = None;
        for option in packet.options().filter(|option| option.kind() == 1) {
            let Ok(mac) = <[u8; 6]>::try_from(&option.bytes()[2..]) else {
                return;
            };
            if source.is_unspecified() || link_address.is_some() || mac == [0; 6] || mac[0] & 1 != 0
            {
                return;
            }
            link_address = Some(mac);
        }
        if let Some(mac) = link_address {
            self.neighbors.learn(source, mac, now);
        }
        let deadline = now
            .saturating_add(random_delay(MAX_RESPONSE_DELAY))
            .max(self.advertiser.earliest());
        if self
            .advertiser
            .next
            .is_none_or(|next| next.deadline().is_some_and(|old| deadline < old))
        {
            self.advertiser.next = Some(SendTime::At(deadline));
        }
    }

    pub(super) fn poll_advertisements(
        &mut self,
        now: u64,
        mtu: usize,
        mut send: impl FnMut(Packet<'_>) -> bool,
    ) {
        let enabled = self.advertiser.joined;
        if !enabled && !self.advertiser.needs_withdrawal() {
            return;
        }
        if self.advertiser.next.is_some_and(|next| !next.ready(now)) {
            return;
        }
        let Some(source) = self.preferred_link() else {
            return;
        };
        let state = &mut self.advertiser;
        let earliest = state.earliest();
        let next = *state.next.get_or_insert_with(|| {
            let initial = if enabled {
                now.saturating_add(random_delay(MAX_RESPONSE_DELAY))
            } else {
                now
            };
            SendTime::At(initial.max(earliest))
        });
        if !next.ready(now) || now < earliest {
            return;
        }
        let (max_interval, lifetime) = match state.policy {
            RouterAdvertisements::Enabled {
                max_interval_seconds,
                router_lifetime_seconds,
                ..
            } if enabled => (
                u64::from(max_interval_seconds) * 1000,
                router_lifetime_seconds,
            ),
            _ => (0, 0),
        };
        let mut prefixes = [(Ipv6Addr::UNSPECIFIED, 0, 0); MAX_ADVERTISED_PREFIXES];
        let mut count = 0;
        if enabled {
            for prefix in self
                .addresses
                .iter()
                .filter(|address| address.usable())
                .filter_map(|address| address.definition().advertised_prefix())
            {
                if count == 0 || prefixes[count - 1].0 != prefix {
                    prefixes[count] = (prefix, PREFERRED_LIFETIME, VALID_LIFETIME); // 候选地址和通告策略在预备时共同验证上界。
                    count += 1;
                }
            }
            let static_count = count;
            for prefix in &self.delegated {
                let valid = remaining(prefix.valid_until, now, VALID_LIFETIME);
                if valid != 0
                    && prefixes[..static_count]
                        .binary_search_by_key(&prefix.address, |entry| entry.0)
                        .is_err()
                {
                    let preferred = remaining(prefix.preferred_until, now, PREFERRED_LIFETIME);
                    prefixes[count] = (prefix.address, preferred.min(valid), valid);
                    count += 1;
                }
            }
            prefixes[..count].sort_unstable_by_key(|entry| entry.0);
        }
        let live = &prefixes[..count];
        state.prefixes.retain(|entry| entry.expires > now);
        for entry in &mut state.prefixes {
            if !entry.stale
                && live
                    .binary_search_by_key(&entry.prefix, |item| item.0)
                    .is_err()
            {
                entry.expires = now.saturating_add(u64::from(VALID_LIFETIME) * 1000);
                entry.stale = true;
            }
        }
        let dns_policy = if enabled {
            state.policy.dns()
        } else {
            Default::default()
        };
        let dns_servers = dns_policy.as_slice();
        let dns_lifetime = (max_interval / 1000) as u32 * 3;
        let mut dns = [[0; 16]; MAX_ADVERTISED_DNS];
        for (output, address) in dns.iter_mut().zip(dns_servers) {
            *output = address.octets();
        }
        let dns = &dns[..dns_servers.len()];
        if state.dns_expires <= now {
            state.dns.clear();
        }
        let mut withdrawn_dns = [[0; 16]; MAX_ADVERTISED_DNS];
        let mut dns_count = 0;
        for address in state
            .dns
            .iter()
            .filter(|address| !dns_servers.contains(address))
        {
            withdrawn_dns[dns_count] = address.octets();
            dns_count += 1;
        }
        let withdrawn_dns = &withdrawn_dns[..dns_count];
        let prefix_capacity =
            (ADVERTISEMENT_OPTION_SPACE - rdnss_size(dns.len()) - rdnss_size(withdrawn_dns.len()))
                / 32;
        let cursor = state.withdraw_after;
        let announced = if cursor.is_none() { live } else { &[] };
        if announced.len() > prefix_capacity {
            // The current prefixes and DNS fit together. Send withdrawals separately when
            // the old DNS addresses would push that set over the minimum link MTU.
            if !send(Packet::RouterAdvertisement {
                source,
                lifetime,
                mtu: mtu as u32,
                prefixes: &[],
                withdrawn: &[],
                dns,
                dns_lifetime,
                withdrawn_dns,
            }) {
                state.next = Some(SendTime::Writable);
                return;
            }
            state.record_dns(dns_servers, now, dns_lifetime);
            state.last = Some((now, true));
            state.next = Some(SendTime::At(now.saturating_add(MIN_MULTICAST_INTERVAL)));
            return;
        }
        let mut withdrawn = [Ipv6Addr::UNSPECIFIED; MAX_ADVERTISED_PREFIXES];
        let capacity = prefix_capacity - announced.len();
        let mut stale = state
            .prefixes
            .iter()
            .map(|entry| entry.prefix)
            .filter(|prefix| {
                cursor.is_none_or(|after| *prefix > after)
                    && live.binary_search_by_key(prefix, |item| item.0).is_err()
            });
        let mut count = 0;
        for prefix in stale.by_ref().take(capacity) {
            withdrawn[count] = prefix;
            count += 1;
        }
        let withdrawn = &withdrawn[..count];
        let remaining = stale.next().is_some();
        if !send(Packet::RouterAdvertisement {
            source,
            lifetime,
            mtu: mtu as u32,
            prefixes: announced,
            withdrawn,
            dns,
            dns_lifetime,
            withdrawn_dns,
        }) {
            state.next = Some(SendTime::Writable);
            return;
        }
        for &(prefix, _, _) in announced {
            let expires = now.saturating_add(u64::from(VALID_LIFETIME) * 1000);
            match state
                .prefixes
                .binary_search_by_key(&prefix, |entry| entry.prefix)
            {
                Ok(index) => {
                    state.prefixes[index] = AnnouncedPrefix {
                        prefix,
                        expires,
                        stale: false,
                    }
                }
                Err(index) => {
                    assert!(
                        state.prefixes.len() < state.prefixes.capacity(),
                        "advertised prefix history prepared"
                    );
                    state.prefixes.insert(
                        index,
                        AnnouncedPrefix {
                            prefix,
                            expires,
                            stale: false,
                        },
                    );
                }
            }
        }
        state.record_dns(dns_servers, now, dns_lifetime);
        state.last = Some((
            now,
            enabled && (lifetime != 0 || !state.prefixes.is_empty() || !state.dns.is_empty()),
        ));
        if enabled {
            state.initial = state.initial.saturating_add(1);
        }
        state.withdraw_after = remaining.then(|| {
            withdrawn
                .last()
                .copied()
                .or(cursor)
                .unwrap_or(Ipv6Addr::UNSPECIFIED)
        });
        if remaining {
            state.next = Some(SendTime::At(now.saturating_add(MIN_MULTICAST_INTERVAL)));
            return;
        }
        if !enabled {
            state.next = None;
            state.initial = 0;
            return;
        }
        let min = (max_interval / 3).max(MIN_MULTICAST_INTERVAL);
        let interval = min + random_delay(max_interval - min);
        let interval = if state.initial < MAX_INITIAL_ADVERTISEMENTS {
            interval.min(MAX_INITIAL_INTERVAL)
        } else {
            interval
        };
        state.next = Some(SendTime::At(now.saturating_add(interval)));
    }
}
