//! 配置服务只持具名域能力、实际接口寿命及待应用工作。期望配置始终来自持久决定。

use super::*;
use alloc::format;
use core::num::NonZeroU128;
use dhcp_server::{PoolExecution, StagedPools};
use dns_cache::DnsExecution;
use network_protocol::bgp::BgpInstance;
use network_protocol::ospf::OspfInstance;
use router_protocol::configuration::{
    Assignment, DeviceMatch, InterfaceReference, Name, RoutingDomain,
};
use router_protocol::interface::{DefaultRoute, DelegatedPrefixes, Ipv6Addresses};
use router_protocol::routing::{Attributes, Family};

mod prefix_delegation;
mod uplink_dhcp;

#[derive(Default)]
pub(super) struct Domains {
    scopes: Vec<Scope>,
    pub(super) retry_at: Option<i64>,
    pub(super) rescan: bool,
}

struct Scope {
    name: Name,
    // 实例身份与配置名分开。旧 Linux 命名空间或 socket 保活时，同名重建不能复用旧 NET。
    instance: NonZeroU128,
    instance_name: Name,
    retired: bool,
    controller: RoutingConfigurator,
    // 单独的 RIB 重启期间由配置 owner 保活 NET 域和现有物理接口。
    network: RemoteNamespace,
    interfaces: Vec<Attached>,
    // 仅配置决定或本域接口事实变化时重放；失败保留工作，不在空闲时轮询。
    pending: bool,
    pools: PoolExecution,
    dns: DnsExecution,
    delegation: delegation::Delegation,
    // Created on first DNS configuration. The NET domain signals when a listener address or
    // scoped upstream becomes available, including after IPv6 duplicate address detection.
    dns_watch: Option<(u64, Event)>,
    bgp: Option<BgpInstance>,
    // 协议失败只重试会话配置，不重编译已确认的静态数据面。
    bgp_pending: bool,
    ospf: Option<OspfInstance>,
    ospf_pending: bool,
}

struct Attached {
    id: u64,
    mac: [u8; 6],
    index: u32,
    lifetime: (u64, Channel),
    link_changed: Event,
    dhcp: Option<(u64, Channel)>,
    lease: Option<DomainLease>,
}

struct DomainLease {
    address: Ipv4Addr,
    prefix_length: u8,
    gateway: Ipv4Addr,
    timing: LeaseTiming,
}

impl DomainLease {
    fn valid_at(&self, now: i64) -> bool {
        self.timing.duration_seconds == u32::MAX
            || now
                < self
                    .timing
                    .requested_at_ns
                    .saturating_add(i64::from(self.timing.duration_seconds) * 1_000_000_000)
    }
}

impl Attached {
    fn profile<'a>(
        &self,
        domain: &'a RoutingDomain,
    ) -> Option<&'a router_protocol::configuration::Interface> {
        domain
            .interfaces
            .values()
            .find(|profile| profile.device == DeviceMatch::Mac(self.mac))
    }

    fn wants_dhcp(&self, domain: &RoutingDomain) -> bool {
        self.profile(domain).is_some_and(|profile| {
            profile.enabled != Some(false)
                && profile
                    .ipv4
                    .as_ref()
                    .is_some_and(|ipv4| ipv4.assignment == Assignment::Dhcp)
        })
    }
}

impl Domains {
    fn find(&self, name: &str) -> Result<&Scope, Status> {
        self.scopes
            .iter()
            .find(|scope| !scope.retired && scope.name.as_str() == name)
            .ok_or(native_abi::ERR_SHOULD_WAIT)
    }

    pub(super) fn namespace(&self, name: &str) -> Result<(NonZeroU128, &RemoteNamespace), Status> {
        let scope = self.find(name)?;
        if scope.pending {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        Ok((scope.instance, &scope.network))
    }

    pub(super) fn network(&self, name: &str) -> Result<&RemoteNamespace, Status> {
        Ok(&self.find(name)?.network)
    }

    pub(super) fn pool_bindings_channel(
        &self,
        domain: &str,
        name: &[u8],
    ) -> Result<Channel, Status> {
        let scope = self.find(domain)?;
        if scope.pending {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let index = scope
            .network
            .interface_index(name)
            .map_err(configuration_status)?;
        scope.pools.bindings_channel(index)
    }

    /// Select the active DHCP client on the named domain's actual interface.
    /// # Errors
    /// The domain or interface is unavailable, or the interface has no current session.
    pub(super) fn dhcp_channel(&self, domain: &str, name: &[u8]) -> Result<&Channel, Status> {
        let scope = self.find(domain)?;
        if scope.pending {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let index = scope
            .network
            .interface_index(name)
            .map_err(configuration_status)?;
        scope
            .interfaces
            .iter()
            .find(|port| port.index == index)
            .and_then(|port| port.dhcp.as_ref().map(|(_, channel)| channel))
            .ok_or(native_abi::ERR_BAD_STATE)
    }

    pub(super) fn ospf_instance(&self, name: &str) -> Result<&OspfInstance, Status> {
        self.find(name)?
            .ospf
            .as_ref()
            .ok_or(native_abi::ERR_SHOULD_WAIT)
    }

    pub(super) fn ospf_closed(&mut self) -> bool {
        let mut changed = false;
        for scope in &mut self.scopes {
            if scope.ospf.as_ref().is_some_and(|instance| {
                instance.channel().wait_one(signals::PEER_CLOSED, 0).is_ok()
            }) {
                scope.restart_ospf();
                changed = true;
            }
        }
        changed
    }

    pub(super) fn bgp_instance(&self, name: &str) -> Result<&BgpInstance, Status> {
        let scope = self.find(name)?;
        if scope.bgp_pending {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        scope.bgp.as_ref().ok_or(native_abi::ERR_SHOULD_WAIT)
    }

    pub(super) fn bgp_closed(&mut self) -> bool {
        let mut changed = false;
        for scope in &mut self.scopes {
            if scope.bgp.as_ref().is_some_and(|instance| {
                instance.channel().wait_one(signals::PEER_CLOSED, 0).is_ok()
            }) {
                scope.restart_bgp();
                changed = true;
            }
        }
        changed
    }

    pub(super) fn controller(&self, name: &str) -> Result<&RoutingConfigurator, Status> {
        Ok(&self.find(name)?.controller)
    }

    fn open(&mut self, root: &RoutingConfigurator, name: &Name) -> Result<&mut Scope, Status> {
        if let Some(position) = self
            .scopes
            .iter()
            .position(|scope| !scope.retired && &scope.name == name)
        {
            let scope = &mut self.scopes[position];
            scope.reconnect(root)?;
            return Ok(scope);
        }
        self.scopes
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut entropy = [0; 16];
        native_runtime::system::cprng_draw(&mut entropy);
        let instance =
            NonZeroU128::new(u128::from_le_bytes(entropy) | 1).expect("nonzero instance");
        let instance_name = Name::try_from(format!("{instance:032x}")).expect("hex instance name");
        let (controller, network) = root.open_routing_domain(&instance_name)?;
        self.scopes.push(Scope {
            name: name.clone(),
            instance,
            instance_name,
            retired: false,
            controller,
            network,
            interfaces: Vec::new(),
            pending: true,
            pools: PoolExecution::default(),
            dns: DnsExecution::default(),
            delegation: delegation::Delegation::default(),
            dns_watch: None,
            bgp: None,
            bgp_pending: true,
            ospf: None,
            ospf_pending: true,
        });
        Ok(self.scopes.last_mut().expect("opened routing domain"))
    }

    fn remove(&mut self, key: u64, configuration: Option<&Configuration>) -> bool {
        for scope in &mut self.scopes {
            if let Some(position) = scope
                .interfaces
                .iter()
                .position(|port| port.lifetime.0 == key)
            {
                scope.remove_interface(position, configuration);
                return true;
            }
        }
        false
    }
}

impl RouterConfig {
    pub(super) fn queue_domains(&self, inner: &mut Inner) {
        inner.domains.retry_at = Some(native_runtime::system::monotonic());
        self.wake_configuration();
    }

    pub(super) fn commit_domains(&self, inner: &mut Inner, previous: Option<&Configuration>) {
        let saved = inner
            .saved
            .as_deref()
            .expect("durable routing configuration");
        for scope in &mut inner.domains.scopes {
            if scope.retired {
                continue;
            }
            let before = previous.and_then(|config| config.routing_domains.get(&scope.name));
            let after = saved.routing_domains.get(&scope.name);
            scope.retired = after.is_none();
            scope.pending |= execution_changed(before, after);
            if before.and_then(|profile| profile.dns_forwarding.as_ref())
                != after.and_then(|profile| profile.dns_forwarding.as_ref())
            {
                scope.dns.retry_at = Some(native_runtime::system::monotonic());
            }
            scope.commit_bgp(before, after);
            if ospf_changed(before, after) {
                scope.restart_ospf();
            }
        }
        if !previous.into_iter().flat_map(bindings).eq(bindings(saved)) {
            inner.domains.rescan = true;
            inner.device_retry_at = Some(native_runtime::system::monotonic());
        }
        self.queue_domains(inner);
    }

    pub(super) fn install_in_domain(
        &self,
        inner: &mut Inner,
        entry: &device_protocol::directory::Entry,
        name: &Name,
        mac: [u8; 6],
    ) -> Result<(), Status> {
        let root = &inner.link.as_ref().ok_or(native_abi::ERR_PEER_CLOSED)?;
        let scope = inner.domains.open(&root.installer, name)?;
        let previous = scope.interfaces.iter().position(|port| port.mac == mac);
        if let Some(position) = previous {
            let port = &scope.interfaces[position];
            if port.lifetime.1.wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT) {
                return if port.id == entry.id {
                    Ok(())
                } else {
                    Err(native_abi::ERR_INVALID_ARGS)
                };
            }
        }
        scope
            .interfaces
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let key = inner.next_key;
        inner.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let (index, lifetime, link_changed) = root.devices.install_device(
            entry.id,
            Some(&scope.instance_name),
            &entry.description,
            self.directory.duplicate()?,
        )?;
        lifetime.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        if let Some(position) = previous {
            scope.remove_interface(position, inner.saved.as_deref());
        }
        scope.interfaces.push(Attached {
            id: entry.id,
            mac,
            index,
            lifetime: (key, lifetime),
            link_changed,
            dhcp: None,
            lease: None,
        });
        scope.pending = true;
        scope.bgp_port_changed(
            inner
                .saved
                .as_ref()
                .and_then(|saved| saved.routing_domains.get(name)),
            mac,
        );
        self.queue_domains(inner);
        Self::changed(inner);
        Ok(())
    }

    pub(super) fn domain_interface_removed(&self, inner: &mut Inner, key: u64) -> bool {
        if !inner.domains.remove(key, inner.saved.as_deref()) {
            return false;
        }
        self.queue_domains(inner);
        Self::changed(inner);
        true
    }

    pub(super) fn domain_pool_closed(&self, inner: &mut Inner, key: u64) -> bool {
        for scope in &mut inner.domains.scopes {
            if let Some(retry) = scope.pools.remove_closed(key) {
                if retry {
                    scope.pools.retry_at = Some(native_runtime::system::monotonic());
                    self.queue_domains(inner);
                    Self::changed(inner);
                }
                return true;
            }
        }
        false
    }

    pub(super) fn domain_dns_event(&self, inner: &mut Inner, key: u64) -> bool {
        for scope in &mut inner.domains.scopes {
            let mut changed = scope.dns.remove_closed(key);
            if let Some((watch_key, event)) = &scope.dns_watch
                && *watch_key == key
            {
                changed = true;
                let result = event
                    .signal(signals::USER_0, 0)
                    .and_then(|()| event.wait_async(&self.port, key, signals::USER_0));
                if let Err(status) = result {
                    error!("router-config: domain DNS observer rearm failed: {status}");
                    scope.dns_watch = None;
                }
            }
            if changed {
                scope.dns.retry_at = Some(native_runtime::system::monotonic());
                self.queue_domains(inner);
                return true;
            }
        }
        false
    }

    pub(super) fn apply_domains(&self, inner: &mut Inner) -> bool {
        let Some(root) = inner.link.as_ref().map(|link| &link.installer) else {
            inner.domains.retry_at = Some(transaction::retry_at());
            return false;
        };
        let mut retry = false;
        if let Some(saved) = &inner.saved {
            for name in saved.routing_domains.keys() {
                if let Err(status) = inner.domains.open(root, name) {
                    retry = true;
                    error!(
                        "router-config: opening routing domain {} pending: {status}",
                        name.as_str()
                    );
                }
            }
        }
        let retired = RoutingDomain::default();
        for scope in &mut inner.domains.scopes {
            // 迁移撤下接口后，重放可能早于寿命通知。先清理已关闭的登记，迟到通知会自然忽略。
            for position in (0..scope.interfaces.len()).rev() {
                let (_, lifetime) = &scope.interfaces[position].lifetime;
                if lifetime.wait_one(signals::PEER_CLOSED, 0).is_ok() {
                    scope.remove_interface(position, inner.saved.as_deref());
                }
            }
            if !scope.pending
                && scope.pools.retry_at.is_none()
                && scope.dns.retry_at.is_none()
                && scope.delegation.retry_at.is_none()
                && !scope.bgp_pending
                && !scope.ospf_pending
            {
                continue;
            }
            let profile = inner
                .saved
                .as_ref()
                .and_then(|saved| saved.routing_domains.get(&scope.name))
                .filter(|_| !scope.retired);
            let result = (|| {
                scope.reconnect(root)?;
                let pools = if scope.pending || scope.pools.retry_at.is_some() {
                    scope.apply(self, profile.unwrap_or(&retired), &mut inner.next_key)
                } else {
                    Ok(())
                };
                if scope.pending {
                    return pools;
                }
                let dns = if scope.dns.retry_at.is_some() {
                    scope.apply_dns(self, profile, &mut inner.next_key)
                } else {
                    Ok(())
                };
                let protocols = scope.apply_protocols(self, profile);
                let delegation = scope.start_delegation(self, profile, &mut inner.next_key);
                pools.and(dns).and(protocols).and(delegation)
            })();
            match result {
                Ok(()) => {}
                Err(status) => {
                    retry = true;
                    error!(
                        "router-config: routing domain {} application pending: {status}",
                        scope.name.as_str()
                    );
                }
            }
        }
        inner.domains.scopes.retain(|scope| {
            scope.pending
                || scope.pools.retry_at.is_some()
                || scope.dns.retry_at.is_some()
                || scope.delegation.retry_at.is_some()
                || scope.bgp_pending
                || scope.ospf_pending
                || !scope.interfaces.is_empty()
                || !scope.retired
        });
        inner.domains.retry_at = retry.then(transaction::retry_at);
        !retry
    }
}

fn bindings(configuration: &Configuration) -> impl Iterator<Item = (&Name, &DeviceMatch)> {
    configuration
        .routing_domains
        .iter()
        .flat_map(|(domain, profile)| {
            profile
                .interfaces
                .values()
                .map(move |interface| (domain, &interface.device))
        })
}

fn execution_changed(before: Option<&RoutingDomain>, after: Option<&RoutingDomain>) -> bool {
    let Some((before, after)) = before.zip(after) else {
        return before.is_some() != after.is_some();
    };
    let RoutingDomain {
        interfaces,
        bridges,
        routes,
        policy_routing,
        firewall,
        route_policy,
        ipv4_forwarding,
        ipv6_forwarding,
        bgp: _,
        ospf: _,
        dns_forwarding: _,
        prefix_delegation,
    } = before;
    *interfaces != after.interfaces
        || *bridges != after.bridges
        || *routes != after.routes
        || *policy_routing != after.policy_routing
        || *firewall != after.firewall
        || *route_policy != after.route_policy
        || *prefix_delegation != after.prefix_delegation
        || *ipv4_forwarding != after.ipv4_forwarding
        || *ipv6_forwarding != after.ipv6_forwarding
}

fn ospf_changed(before: Option<&RoutingDomain>, after: Option<&RoutingDomain>) -> bool {
    let previous = before.and_then(|domain| domain.ospf.as_ref());
    let next = after.and_then(|domain| domain.ospf.as_ref());
    previous != next
        || before.zip(after).is_some_and(|(before, after)| {
            next.is_some_and(|ospf| {
                before.ipv4_forwarding != after.ipv4_forwarding
                    || before.ipv6_forwarding != after.ipv6_forwarding
                    || ospf.links.keys().any(|name| {
                        before.interface_reference(name) != after.interface_reference(name)
                    })
            })
        })
}

fn domain_default_route(
    profile: &router_protocol::configuration::Interface,
    gateway: Ipv4Addr,
    protocol: u8,
) -> Option<DefaultRoute> {
    (!gateway.is_unspecified()).then(|| DefaultRoute {
        gateway,
        protocol,
        attributes: profile.default_route.unwrap_or(Attributes {
            preference: 1,
            metric: 0,
        }),
    })
}

impl Scope {
    fn remove_interface(&mut self, position: usize, configuration: Option<&Configuration>) {
        let removed = self.interfaces.remove(position);
        self.delegation.remove_interface(removed.index);
        self.pools.remove_interface(removed.index);
        self.pending = true;
        if !self.retired {
            self.bgp_port_changed(
                configuration.and_then(|config| config.routing_domains.get(&self.name)),
                removed.mac,
            );
        }
    }

    fn reconnect(&mut self, root: &RoutingConfigurator) -> Result<(), Status> {
        if self
            .controller
            .channel()
            .wait_one(signals::PEER_CLOSED, 0)
            .is_ok()
        {
            self.controller = root.open_routing_domain(&self.instance_name)?.0;
            self.pending = true;
            self.restart_bgp();
            self.restart_ospf();
        }
        Ok(())
    }

    fn restart_bgp(&mut self) {
        self.bgp = None;
        self.bgp_pending = true;
    }

    fn restart_ospf(&mut self) {
        self.ospf = None;
        self.ospf_pending = true;
    }

    fn apply_protocols(
        &mut self,
        owner: &RouterConfig,
        profile: Option<&RoutingDomain>,
    ) -> Result<(), Status> {
        let bgp = if self.bgp_pending {
            let result = self.apply_bgp(owner, profile);
            self.bgp_pending = result.is_err();
            result
        } else {
            Ok(())
        };
        let ospf = if self.ospf_pending {
            let result = self.apply_ospf(owner, profile);
            self.ospf_pending = result.is_err();
            result
        } else {
            Ok(())
        };
        bgp.and(ospf)
    }

    fn apply_ospf(
        &mut self,
        owner: &RouterConfig,
        profile: Option<&RoutingDomain>,
    ) -> Result<(), Status> {
        let session = profile
            .map(RoutingDomain::ospf_session)
            .transpose()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?
            .flatten();
        self.ospf = session
            .map(|session| owner.start_ospf(&self.controller, session))
            .transpose()?;
        Ok(())
    }

    fn bgp_port_changed(&mut self, profile: Option<&RoutingDomain>, mac: [u8; 6]) {
        let uses_port = profile.is_some_and(|profile| {
            profile.bgp.as_ref().is_some_and(|bgp| {
                bgp.neighbors
                    .values()
                    .filter_map(|peer| peer.interface.as_ref())
                    .any(|name| match profile.interface_reference(name) {
                        Some(InterfaceReference::Device(device)) => {
                            *device == DeviceMatch::Mac(mac)
                        }
                        Some(InterfaceReference::Vlan(name)) => {
                            profile.bridges.values().any(|bridge| {
                                bridge
                                    .gateways
                                    .values()
                                    .any(|gateway| gateway.name == *name)
                                    && bridge.ports.keys().any(|name| {
                                        profile.interfaces[name].device == DeviceMatch::Mac(mac)
                                    })
                            })
                        }
                        None => false,
                    })
            })
        });
        if uses_port {
            self.restart_bgp();
        }
    }

    fn commit_bgp(&mut self, before: Option<&RoutingDomain>, after: Option<&RoutingDomain>) {
        let previous = before.and_then(|domain| domain.bgp.as_ref());
        let next = after.and_then(|domain| domain.bgp.as_ref());
        let bindings_changed = before.zip(after).is_some_and(|(before, after)| {
            next.is_some_and(|bgp| {
                bgp.neighbors
                    .values()
                    .filter_map(|peer| peer.interface.as_ref())
                    .any(|name| {
                        let binding = after.interface_reference(name);
                        before.interface_reference(name) != binding
                            || matches!(binding, Some(InterfaceReference::Vlan(_)))
                                && before.bridges != after.bridges
                    })
            })
        });
        if bindings_changed
            || previous != next
                && !previous
                    .zip(next)
                    .is_some_and(|(old, new)| old.supports_live_update(new))
        {
            self.restart_bgp();
        } else if previous != next {
            self.bgp_pending = true;
        }
    }

    fn apply_bgp(
        &mut self,
        owner: &RouterConfig,
        profile: Option<&RoutingDomain>,
    ) -> Result<(), Status> {
        let Some((profile, bgp)) =
            profile.and_then(|profile| profile.bgp.as_ref().map(|bgp| (profile, bgp)))
        else {
            self.bgp = None;
            return Ok(());
        };
        if let Some(instance) = self.bgp.as_ref().filter(|instance| {
            instance.channel().wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        }) {
            return instance.update_neighbors(bgp);
        }
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(bgp.neighbors.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for peer in bgp.neighbors.values() {
            let index = match peer
                .interface
                .as_ref()
                .and_then(|name| profile.interface_reference(name))
            {
                Some(InterfaceReference::Device(device)) => self.device_index(device).unwrap_or(0),
                Some(InterfaceReference::Vlan(name)) => {
                    match self.network.interface_index(name.as_str().as_bytes()) {
                        Ok(index) => index,
                        Err(SocketError::NoDevice) => 0,
                        Err(error) => return Err(configuration_status(error)),
                    }
                }
                None => 0,
            };
            indices.push(index);
        }
        self.bgp = Some(owner.start_bgp(&self.controller, bgp, Some(&indices))?);
        Ok(())
    }

    fn apply(
        &mut self,
        owner: &RouterConfig,
        domain: &RoutingDomain,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        if self.pending {
            self.dns.pause()?;
            if domain.dns_forwarding.is_some() {
                self.dns.retry_at = Some(native_runtime::system::monotonic());
            }
            return self.apply_network(owner, domain, next_key);
        }
        let snapshots = self
            .network
            .interface_snapshots()
            .map_err(configuration_status)?;
        let mut desired = self.physical_pools(domain)?;
        dhcp_server::append_vlan_pools(&mut desired, domain.bridges.values(), |name| {
            dhcp_server::active_vlan_binding(&snapshots, name)
        })?;
        let pools = StagedPools::prepare(&mut self.pools, desired, |index| {
            snapshots.iter().any(|snapshot| snapshot.index == index)
        })?;
        self.apply_pools(owner, pools, next_key)
    }

    fn physical_pools(
        &self,
        domain: &RoutingDomain,
    ) -> Result<Vec<(u32, router_protocol::dhcp_server::Pool)>, Status> {
        let mut desired = Vec::new();
        for port in &self.interfaces {
            let profile = domain.interfaces.values().find(|profile| {
                profile.device == DeviceMatch::Mac(port.mac) && profile.enabled != Some(false)
            });
            if let Some(profile) = profile
                && let Some(pool) = profile
                    .dhcp_pool()
                    .map_err(|_| native_abi::ERR_INVALID_ARGS)?
            {
                desired
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                desired.push((port.index, pool));
            }
        }
        Ok(desired)
    }

    fn apply_pools(
        &mut self,
        owner: &RouterConfig,
        pools: StagedPools,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let result = pools.apply(owner, &mut self.pools, next_key, &self.network);
        self.pools.retry_at = result.is_err().then(transaction::retry_at);
        result
    }

    fn device_index(&self, device: &DeviceMatch) -> Option<u32> {
        self.interfaces
            .iter()
            .find(|port| *device == DeviceMatch::Mac(port.mac))
            .map(|port| port.index)
    }

    fn apply_dns(
        &mut self,
        owner: &RouterConfig,
        profile: Option<&RoutingDomain>,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let forwarding = profile.and_then(|profile| profile.dns_forwarding.as_ref());
        if forwarding.is_none() {
            self.dns_watch = None;
        }
        let result = (|| {
            if forwarding.is_some() && self.dns_watch.is_none() {
                let event = self.network.watch_dns().map_err(configuration_status)?;
                let key = *next_key;
                *next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                event.wait_async(&owner.port, key, signals::USER_0)?;
                self.dns_watch = Some((key, event));
            }
            let settings = dns_cache::compile(&self.network, forwarding, |name| {
                profile?.interface_reference(name)
            })?;
            owner.apply_dns_settings(&mut self.dns, next_key, &self.network, settings)
        })();
        self.dns.retry_at = result.is_err().then(transaction::retry_at);
        result
    }

    fn apply_network(
        &mut self,
        owner: &RouterConfig,
        domain: &RoutingDomain,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(self.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let now = native_runtime::system::monotonic();
        let retain_delegation = self.retains_delegation(domain);
        let assignments = self.delegated_assignments(domain, retain_delegation, now)?;
        for (port, prefixes) in self.interfaces.iter().zip(&assignments) {
            let profile = port.profile(domain);
            let up = profile.is_some_and(|profile| profile.enabled.unwrap_or(true));
            let ipv4 = profile
                .filter(|_| up)
                .and_then(|profile| profile.ipv4.as_ref())
                .filter(|ipv4| ipv4.assignment == Assignment::Static);
            let lease = port
                .lease
                .as_ref()
                .filter(|lease| port.wants_dhcp(domain) && lease.valid_at(now));
            let ipv6 = profile.and_then(|profile| profile.ipv6.as_ref());
            plans.push(InterfaceConfiguration {
                index: port.index,
                mtu: profile.map(|profile| profile.mtu),
                up,
                address: ipv4
                    .map(|address| address.address)
                    .or_else(|| lease.map(|lease| lease.address)),
                prefix_length: ipv4
                    .map(|address| address.prefix_length)
                    .or_else(|| lease.map(|lease| lease.prefix_length))
                    .unwrap_or(0),
                route: profile.and_then(|profile| {
                    if let Some(ipv4) = ipv4 {
                        domain_default_route(profile, ipv4.gateway, RTPROT_BOOT)
                    } else {
                        lease.and_then(|lease| {
                            domain_default_route(profile, lease.gateway, RTPROT_DHCP)
                        })
                    }
                }),
                ipv6: Some(
                    Ipv6Addresses::new(ipv6.map_or(&[][..], |policy| policy.addresses.as_slice()))
                        .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                ),
                delegated: Some(
                    DelegatedPrefixes::new(prefixes).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                ),
                accept_ra: Some(ipv6.map_or_else(Default::default, |policy| policy.accept_ra)),
                advertise: Some(ipv6.map_or_else(Default::default, |policy| policy.advertise)),
            });
        }
        let bridges = domain
            .bridge_source(|device| Ok(self.device_index(device)))
            .map_err(routing::encode_status)?;
        let bridges = routing::project_bridges(&self.controller, bridges, &plans)?;
        let ports =
            router_protocol::switching::Ports::parse(&bridges).map_err(routing::encode_status)?;
        let vlan_assignments = self.vlan_assignments(domain, ports, retain_delegation, now)?;
        plans
            .try_reserve_exact(vlan_assignments.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (gateway, prefixes) in &vlan_assignments {
            plans.push(delegation::prefix_plan(
                (
                    gateway.binding.index,
                    true,
                    gateway.address,
                    gateway.prefix_length,
                ),
                prefixes,
                None,
            )?);
        }
        let resolve = |reference: InterfaceReference<'_>| match reference {
            InterfaceReference::Device(device) => Ok(self.device_index(device)),
            InterfaceReference::Vlan(name) => routing::resolve_vlan(Some(ports), name),
        };
        let ipv4 = domain
            .route_source(Family::Ipv4, resolve)
            .map_err(routing::encode_status)?;
        let ipv6 = domain
            .route_source(Family::Ipv6, resolve)
            .map_err(routing::encode_status)?;
        let policy_routing = domain
            .policy_source(resolve)
            .map_err(routing::encode_status)?;
        let firewall = domain
            .firewall_source(resolve)
            .map_err(routing::encode_status)?;
        let prerouting = router_protocol::firewall::Prerouting::parse(&firewall)
            .expect("encoded firewall policy");
        let mut started = Vec::new();
        started
            .try_reserve_exact(self.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut desired = self.physical_pools(domain)?;
        dhcp_server::append_vlan_pools(&mut desired, domain.bridges.values(), |name| {
            let binding = ports.gateway(name.as_str().as_bytes())?.binding;
            (binding.index != 0).then_some((binding.index, u32::from(binding.mtu)))
        })?;
        let mut pools = StagedPools::prepare(&mut self.pools, desired, |index| {
            self.interfaces.iter().any(|port| port.index == index)
                || ports
                    .gateways()
                    .any(|gateway| gateway.binding.index == index)
        })?;
        let result = (|| {
            let transaction =
                transaction::activate_execution(self.controller.prepare_configuration(
                    &plans,
                    [Some(&ipv4), Some(&ipv6)],
                    Some(&domain.route_policy),
                    Some(&policy_routing),
                    network_protocol::ExecutionPolicy {
                        forwarding: Some(network_protocol::Forwarding {
                            ipv4: domain.ipv4_forwarding,
                            ipv6: domain.ipv6_forwarding,
                        }),
                        prerouting: Some(prerouting),
                        bridges: Some(ports),
                        ..Default::default()
                    },
                ))
                .map_err(StageError::status)?;
            for (position, port) in self.interfaces.iter().enumerate() {
                if !port.wants_dhcp(domain) || port.dhcp.is_some() {
                    continue;
                }
                let result = (|| {
                    let key = *next_key;
                    *next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                    Ok((
                        key,
                        owner.open_dhcp(&self.network, port.index, port.mac, key)?,
                    ))
                })();
                match result {
                    Ok(session) => started.push((position, session)),
                    Err(status) => {
                        started.clear();
                        return Err(transaction::abort_execution(transaction, status).status());
                    }
                }
            }
            transaction.confirm().map_err(configuration_status)
        })();
        if let Err(status) = result {
            // 配置已持久。上一次执行结果不明时保留旧池的暂停，直到本域重放成功。
            pools.hold(&mut self.pools);
            return Err(status);
        }
        self.pending = false;
        if !retain_delegation {
            self.delegation.retire();
        }
        self.delegation.confirm_projection();
        for port in &mut self.interfaces {
            if !port.wants_dhcp(domain) {
                port.dhcp = None;
                port.lease = None;
            }
        }
        for (position, session) in started {
            self.interfaces[position].dhcp = Some(session);
        }
        self.apply_pools(owner, pools, next_key)
    }
}
