//! RFC 4861/4862/4191 主机发现。通告寿命及地址资格只归接收接口，FIB 从这里借用候选。
use super::*;
use packet_wire::ndisc::options::{OptionValue, RoutePreference};
use prefix_index::Address as _;

const MAX_DEFAULT_ROUTES: usize = 8;
const MAX_SPECIFIC_ROUTES: usize = 24;
const MAX_PREFIXES: usize = 16;
const MAX_ADDRESSES: usize = 16;
const TWO_HOURS: u64 = 7_200_000;

const DEFAULT_PREFIX: (Ipv6Addr, u8) = (Ipv6Addr::UNSPECIFIED, 0);

struct GatewayRoute {
    prefix: (Ipv6Addr, u8),
    gateway: Ipv6Addr,
    expires: u64,
    preference: RoutePreference,
}
impl GatewayRoute {
    fn priority(&self) -> (u8, u8) {
        (
            self.prefix.1,
            match self.preference {
                RoutePreference::High => 2,
                RoutePreference::Medium => 1,
                RoutePreference::Low => 0,
            },
        )
    }
}
struct Prefix {
    network: Ipv6Addr,
    length: u8,
    expires: u64,
}
pub(super) struct Autonomous {
    // 手工地址暂时持有 DAD 资格时，这里的 Address 保持 Off，通告寿命继续独立更新。
    pub(super) address: Address,
    preferred_until: u64,
    valid_until: u64,
}
impl Autonomous {
    /// 剩余不足一秒仍按一秒显示，保留零表示已到期。
    pub(super) fn remaining(&self, now: u64) -> (u32, u32) {
        let seconds = |until: u64| {
            if until == u64::MAX {
                u32::MAX
            } else {
                until.saturating_sub(now).div_ceil(1000) as u32
            }
        };
        (seconds(self.preferred_until), seconds(self.valid_until))
    }
}
/// 一条动态候选的当前事实。期限由原接口 owner 处理，不复制到静态索引。
#[derive(Clone, Copy)]
pub(in crate::inet) struct LearnedRoute {
    pub(in crate::inet) network: Ipv6Addr,
    pub(in crate::inet) length: u8,
    pub(in crate::inet) gateway: Option<Ipv6Addr>,
    pub(in crate::inet) metric: u32,
    pub(in crate::inet) local: bool,
    pub(in crate::inet) protocol: u8,
}
pub(super) struct Discovery {
    gateway_routes: Vec<GatewayRoute>,
    prefixes: Vec<Prefix>,
    pub(super) addresses: Vec<Autonomous>,
    // 通告只限制 IPv6。零表示尚无通告，实际链路 MTU 仍由设备决定。
    advertised_mtu: u32,
    hop_limit: u8,
    pub(super) dns: super::dns::Dns,
    // OWNER: None 表示尚未启动主机发现。只计实际已发送的 RS，收到 RA 后停止重试。
    solicitation: Option<(SendTime, u8)>,
}
impl Discovery {
    pub(super) fn new() -> Self {
        Self {
            gateway_routes: Vec::new(),
            prefixes: Vec::new(),
            addresses: Vec::new(),
            advertised_mtu: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            dns: super::dns::Dns::default(),
            solicitation: None,
        }
    }

    pub(super) fn due(&self, now: u64) -> bool {
        self.dns.due(now)
            || self.gateway_routes.iter().any(|route| route.expires <= now)
            || self.prefixes.iter().any(|prefix| prefix.expires <= now)
            || self.addresses.iter().any(|row| {
                row.valid_until <= now || row.address.preferred() && row.preferred_until <= now
            })
            || self.solicitation.is_some_and(|(at, remaining)| {
                remaining != 0 && at.deadline().is_some_and(|at| at <= now)
            })
    }

    pub(super) fn expire(&mut self, now: u64) -> bool {
        let before = (
            self.gateway_routes.len(),
            self.prefixes.len(),
            self.addresses.len(),
        );
        self.gateway_routes.retain(|route| route.expires > now);
        self.prefixes.retain(|prefix| prefix.expires > now);
        self.addresses.retain(|row| row.valid_until > now);
        let deprecated = self.refresh_preference(now);
        self.dns.expire(now) | deprecated
            || before
                != (
                    self.gateway_routes.len(),
                    self.prefixes.len(),
                    self.addresses.len(),
                )
    }

    pub(super) fn refresh_preference(&mut self, now: u64) -> bool {
        let mut changed = false;
        for row in &mut self.addresses {
            changed |= row.address.set_deprecated(row.preferred_until <= now);
        }
        changed
    }

    pub(super) fn poll_solicit(
        &mut self,
        accept_ra: bool,
        source: Option<Ipv6Addr>,
        now: u64,
        mut send: impl FnMut(Packet<'_>) -> bool,
    ) {
        if !accept_ra {
            self.solicitation = None;
            return;
        }
        let source = source.unwrap_or(Ipv6Addr::UNSPECIFIED);
        let (at, remaining) = self
            .solicitation
            .get_or_insert_with(|| (SendTime::At(now.saturating_add(random_delay(1000))), 3));
        if *remaining == 0 || !at.ready(now) {
            return;
        }
        if send(Packet::RouterSolicitation { source }) {
            *remaining -= 1;
            *at = SendTime::At(now.saturating_add(4000));
        } else {
            *at = SendTime::Writable;
        }
    }

    pub(super) fn remove_router(&mut self, address: Ipv6Addr) -> bool {
        let previous = self.gateway_routes.len();
        self.gateway_routes.retain(|route| route.gateway != address);
        previous != self.gateway_routes.len()
    }
    pub(super) fn has_gateway(&self, gateway: Ipv6Addr) -> bool {
        self.gateway_routes
            .iter()
            .any(|route| route.gateway == gateway)
    }

    fn update_gateway_route(
        &mut self,
        prefix: (Ipv6Addr, u8),
        gateway: Ipv6Addr,
        lifetime: u32,
        preference: RoutePreference,
        now: u64,
    ) -> bool {
        let index = self
            .gateway_routes
            .iter()
            .position(|route| route.prefix == prefix && route.gateway == gateway);
        if lifetime == 0 {
            return index.is_some_and(|index| {
                self.gateway_routes.remove(index);
                true
            });
        }
        let expires = deadline(now, lifetime);
        if let Some(index) = index {
            let route = &mut self.gateway_routes[index];
            let changed = route.preference != preference;
            route.expires = expires;
            route.preference = preference;
            return changed;
        }
        let default = prefix == DEFAULT_PREFIX;
        let limit = if default {
            MAX_DEFAULT_ROUTES
        } else {
            MAX_SPECIFIC_ROUTES
        };
        let count = self
            .gateway_routes
            .iter()
            .filter(|route| (route.prefix == DEFAULT_PREFIX) == default)
            .count();
        if count == limit || self.gateway_routes.try_reserve(1).is_err() {
            return false;
        }
        self.gateway_routes.push(GatewayRoute {
            prefix,
            gateway,
            expires,
            preference,
        });
        true
    }

    fn prefix(&mut self, network: Ipv6Addr, length: u8, valid: u32, now: u64) -> bool {
        let index = self
            .prefixes
            .iter()
            .position(|prefix| prefix.network == network && prefix.length == length);
        if valid == 0 {
            return index.is_some_and(|index| {
                self.prefixes.remove(index);
                true
            });
        }
        let expires = deadline(now, valid);
        if let Some(index) = index {
            self.prefixes[index].expires = expires;
            return false;
        }
        if self.prefixes.len() == MAX_PREFIXES || self.prefixes.try_reserve(1).is_err() {
            return false;
        }
        self.prefixes.push(Prefix {
            network,
            length,
            expires,
        });
        true
    }

    pub(super) fn resume_address(&mut self, previous: &mut Address, now: u64) {
        if let Some(row) = self
            .addresses
            .iter_mut()
            .find(|row| row.address.address == previous.address && row.valid_until > now)
        {
            row.address.exchange_state(previous);
            row.address.start(now);
            row.address.set_deprecated(row.preferred_until <= now);
        }
    }

    fn autonomous(
        &mut self,
        address: Ipv6Addr,
        valid: u32,
        preferred: u32,
        now: u64,
        configured: bool,
    ) -> bool {
        let valid_until = deadline(now, valid);
        let preferred_until = deadline(now, preferred);
        if let Some(row) = self
            .addresses
            .iter_mut()
            .find(|row| row.address.address == address)
        {
            // RFC 4862 §5.5.3(e) 的未经认证通告保护。优选期独立更新。
            let advertised = valid_until.saturating_sub(now);
            let remaining = row.valid_until.saturating_sub(now);
            if advertised > TWO_HOURS || advertised > remaining {
                row.valid_until = valid_until;
            } else if remaining > TWO_HOURS {
                row.valid_until = now.saturating_add(TWO_HOURS);
            }
            row.preferred_until = preferred_until;
            return row.address.set_deprecated(preferred_until <= now);
        }
        if valid == 0
            || self.addresses.len() == MAX_ADDRESSES
            || self.addresses.try_reserve(1).is_err()
        {
            return false;
        }
        let mut address = Address::new(address, 64, false);
        if !configured {
            address.start(now);
        }
        self.addresses.push(Autonomous {
            address,
            preferred_until,
            valid_until,
        });
        !configured
    }

    pub(super) fn routes(&self, now: u64) -> impl Iterator<Item = LearnedRoute> + Clone {
        self.addresses
            .iter()
            .filter(move |row| row.valid_until > now && row.address.usable())
            .map(|row| LearnedRoute {
                network: row.address.address,
                length: 128,
                gateway: None,
                metric: 0,
                local: true,
                protocol: 2,
            })
            .chain(
                self.prefixes
                    .iter()
                    .filter(move |prefix| prefix.expires > now)
                    .map(|prefix| LearnedRoute {
                        network: prefix.network,
                        length: prefix.length,
                        gateway: None,
                        metric: 256,
                        local: false,
                        protocol: 9,
                    }),
            )
            .chain(
                self.gateway_routes
                    .iter()
                    .filter(move |route| route.expires > now)
                    .map(|route| LearnedRoute {
                        network: route.prefix.0,
                        length: route.prefix.1,
                        gateway: Some(route.gateway),
                        metric: 1026 - u32::from(route.priority().1),
                        local: false,
                        protocol: 9,
                    }),
            )
    }
}

pub(super) fn deadline(now: u64, seconds: u32) -> u64 {
    if seconds == u32::MAX {
        u64::MAX
    } else {
        now.saturating_add(u64::from(seconds) * 1000)
    }
}

impl InterfaceAddresses {
    /// 当前配置的通告接收策略，供事务准备逆向交换。
    pub(in crate::inet) fn accept_ra_policy(&self) -> RouterAdvertisementPolicy {
        self.accept_ra
    }

    pub(super) fn accepts_router_advertisements(&self, router: bool) -> bool {
        match self.accept_ra {
            RouterAdvertisementPolicy::HostOnly => !router,
            RouterAdvertisementPolicy::Disabled => false,
            RouterAdvertisementPolicy::Always => true,
        }
    }

    /// 在激活或回滚时交换策略。重新允许通告后可以主动请求，已学习事实保留原寿命。
    pub(in crate::inet) fn exchange_accept_ra(&mut self, policy: &mut RouterAdvertisementPolicy) {
        core::mem::swap(&mut self.accept_ra, policy);
        self.discovery.solicitation = None;
    }

    pub(super) fn receive_advertisement(
        &mut self,
        packet: NdPacket<'_>,
        source: Ipv6Addr,
        now: u64,
        device_mtu: usize,
    ) -> bool {
        let NdMessage::RouterAdvertisement {
            lifetime,
            flags,
            reachable_time,
            retransmit_timer,
            hop_limit,
            ..
        } = packet.message()
        else {
            return false;
        };
        if self.listener.is_none()
            || !source.is_unicast_link_local()
            || self
                .all_addresses()
                .any(|address| address.address == source)
        {
            return false;
        }
        let mut source_link = None;
        for option in packet.options() {
            if option.kind() == 1 {
                let Ok(mac) = <[u8; 6]>::try_from(&option.bytes()[2..]) else {
                    return false;
                };
                if source_link.is_some() || mac == [0; 6] || mac[0] & 1 != 0 {
                    return false;
                }
                source_link = Some(mac);
            }
        }
        self.discovery.solicitation = Some((SendTime::At(now), 0));
        if hop_limit != 0 {
            self.discovery.hop_limit = hop_limit;
        }
        self.neighbors
            .apply_timers(reachable_time, retransmit_timer);
        self.neighbors.learn_router(source, source_link, now);
        let mut changed = self.discovery.expire(now);
        let mut default = (
            u32::from(lifetime),
            match (flags >> 3) & 3 {
                1 => RoutePreference::High,
                3 => RoutePreference::Low,
                _ => RoutePreference::Medium,
            },
        );
        let mut dns_insertion = (0, 0);
        for option in packet.options() {
            let Ok(option) = option.decode() else {
                continue;
            };
            changed |= self.discovery.dns.receive(option, now, &mut dns_insertion);
            if let OptionValue::Mtu(mtu) = option {
                if mtu >= 1280 && mtu as usize <= device_mtu {
                    self.discovery.advertised_mtu = mtu;
                }
                continue;
            }
            if let OptionValue::RouteInformation {
                length,
                preference,
                lifetime,
                prefix,
            } = option
            {
                let network = Ipv6Addr::from(u128::from(prefix).masked(length));
                if length == 0 {
                    // 同一通告的 ::/0 选项覆盖头部寿命和优先级，只发布最终结果。
                    default = (lifetime, preference);
                } else if !network.is_unicast_link_local()
                    && !network.is_multicast()
                    && network.to_ipv4_mapped().is_none()
                {
                    changed |= self.discovery.update_gateway_route(
                        (network, length),
                        source,
                        lifetime,
                        preference,
                        now,
                    );
                }
                continue;
            }
            let OptionValue::Prefix {
                length,
                flags,
                valid_lifetime,
                preferred_lifetime,
                prefix,
            } = option
            else {
                continue;
            };
            if length > 128 || prefix.is_unicast_link_local() || prefix.is_multicast() {
                continue;
            }
            let network = Ipv6Addr::from(u128::from(prefix).masked(length));
            if flags & 0x80 != 0 {
                changed |= self.discovery.prefix(network, length, valid_lifetime, now);
            }
            if flags & 0x40 != 0
                && length == 64
                && preferred_lifetime <= valid_lifetime
                && !network.is_unspecified()
                && network.to_ipv4_mapped().is_none()
            {
                let address = Ipv6Addr::from(
                    u128::from(network) | u128::from(self.local.address) & u128::from(u64::MAX),
                );
                let configured = self
                    .addresses
                    .iter()
                    .any(|configured| configured.address == address);
                changed |= self.discovery.autonomous(
                    address,
                    valid_lifetime,
                    preferred_lifetime,
                    now,
                    configured,
                );
            }
        }
        changed |=
            self.discovery
                .update_gateway_route(DEFAULT_PREFIX, source, default.0, default.1, now);
        if changed {
            self.synchronize_groups(now);
        }
        changed
    }

    pub(in crate::inet) fn learned_routes(
        &self,
        now: u64,
    ) -> impl Iterator<Item = LearnedRoute> + Clone {
        self.discovery.routes(now).chain(
            self.delegated
                .iter()
                .filter(move |prefix| super::delegation_valid(prefix, now))
                .map(|prefix| LearnedRoute {
                    network: prefix.address,
                    length: prefix.length,
                    gateway: None,
                    metric: 0,
                    local: false,
                    protocol: 16,
                }),
        )
    }

    /// 返回 IPv6 链路 MTU。`device_mtu` 是设备当前限制，通告无法扩大它。
    pub(in crate::inet) fn link_mtu(&self, device_mtu: usize) -> usize {
        if self.discovery.advertised_mtu == 0 {
            device_mtu
        } else {
            device_mtu.min(self.discovery.advertised_mtu as usize)
        }
    }

    /// 当前接口本机单播的默认 IPv6 跳数。
    pub(in crate::inet) fn hop_limit(&self) -> u8 {
        self.discovery.hop_limit
    }
    /// 合并本接口尚有效的 DNS 通告。分配失败不发布部分域快照。
    pub(in crate::inet) fn capture_dns(
        &self,
        index: u32,
        now: u64,
        output: &mut router_protocol::dns::LearnedDns,
    ) -> Result<(), network_protocol::SocketError> {
        self.discovery.dns.capture(index, now, output)
    }

    pub(in crate::inet) fn router_rank(&self, router: Ipv6Addr) -> (bool, bool) {
        self.neighbors.router_rank(router)
    }

    /// 读取失败网关是否可安排下一次探测。选路查询不推进 NUD。
    pub(in crate::inet) fn router_probe_due(&self, router: Ipv6Addr, now: u64) -> bool {
        self.neighbors.router_probe_due(router, now)
    }

    /// 实际提交数据后，在首选网关所属接口登记探测。
    pub(in crate::inet) fn probe_router(&mut self, router: Ipv6Addr, now: u64) {
        self.neighbors.request_router_probe(router, now);
    }
}
