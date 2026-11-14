//! 配置服务只持具名域能力、实际接口寿命及待应用工作。期望配置始终来自持久决定。

use super::*;
use alloc::format;
use core::num::NonZeroU128;
use network_protocol::bgp::BgpInstance;
use network_protocol::ospf::OspfInstance;
use router_protocol::configuration::{DeviceMatch, InterfaceReference, Name, RoutingDomain};
use router_protocol::interface::{DefaultRoute, Ipv6Addresses};
use router_protocol::routing::{Attributes, Family};

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
                let removed = scope.interfaces.remove(position);
                scope.pending = true;
                if !scope.retired {
                    scope.bgp_port_changed(
                        configuration.and_then(|config| config.routing_domains.get(&scope.name)),
                        removed.mac,
                    );
                }
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
        let root = &inner
            .link
            .as_ref()
            .ok_or(native_abi::ERR_PEER_CLOSED)?
            .installer;
        let scope = inner.domains.open(root, name)?;
        for port in &scope.interfaces {
            if port.mac == mac
                && port.lifetime.1.wait_one(signals::PEER_CLOSED, 0)
                    == Err(native_abi::ERR_TIMED_OUT)
            {
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
        let mut delegated = Vec::new();
        delegated
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        delegated.push(self.directory.duplicate()?.into_handle());
        let (index, lifetime, _) = root.install_device(
            entry.id,
            Some(&scope.instance_name),
            &entry.description,
            delegated,
        )?;
        lifetime.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        scope.interfaces.retain(|port| port.mac != mac);
        scope.interfaces.push(Attached {
            id: entry.id,
            mac,
            index,
            lifetime: (key, lifetime),
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
        let retired = RoutingDomain {
            ipv4_forwarding: false,
            ipv6_forwarding: false,
            ..RoutingDomain::default()
        };
        for scope in &mut inner.domains.scopes {
            if !scope.pending && !scope.bgp_pending && !scope.ospf_pending {
                continue;
            }
            let profile = inner
                .saved
                .as_ref()
                .and_then(|saved| saved.routing_domains.get(&scope.name))
                .filter(|_| !scope.retired);
            let result = (|| {
                scope.reconnect(root)?;
                if scope.pending {
                    scope.apply(profile.unwrap_or(&retired))?;
                    scope.pending = false;
                }
                scope.apply_protocols(self, profile)
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
        routes,
        policy_routing,
        firewall,
        route_policy,
        ipv4_forwarding,
        ipv6_forwarding,
        bgp: _,
        ospf: _,
    } = before;
    *interfaces != after.interfaces
        || *routes != after.routes
        || *policy_routing != after.policy_routing
        || *firewall != after.firewall
        || *route_policy != after.route_policy
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
                        before.interfaces.get(name).map(|link| &link.device)
                            != after.interfaces.get(name).map(|link| &link.device)
                    })
            })
        })
}

impl Scope {
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
                    .any(|name| {
                        profile
                            .interfaces
                            .get(name)
                            .is_some_and(|port| port.device == DeviceMatch::Mac(mac))
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
                        before.interfaces.get(name).map(|port| &port.device)
                            != after.interfaces.get(name).map(|port| &port.device)
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
            let index = peer
                .interface
                .as_ref()
                .and_then(|name| profile.interfaces.get(name))
                .and_then(|interface| {
                    self.interfaces
                        .iter()
                        .find(|port| interface.device == DeviceMatch::Mac(port.mac))
                })
                .map_or(0, |port| port.index);
            indices.push(index);
        }
        self.bgp = Some(owner.start_bgp(&self.controller, bgp, Some(&indices))?);
        Ok(())
    }

    fn apply(&self, domain: &RoutingDomain) -> Result<(), Status> {
        let resolve = |reference: InterfaceReference<'_>| {
            let InterfaceReference::Device(DeviceMatch::Mac(mac)) = reference else {
                return Err(router_protocol::Invalid);
            };
            Ok(self
                .interfaces
                .iter()
                .find(|port| port.mac == *mac)
                .map(|port| port.index))
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
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(self.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for port in &self.interfaces {
            let profile = domain
                .interfaces
                .values()
                .find(|profile| profile.device == DeviceMatch::Mac(port.mac));
            let up = profile.is_some_and(|profile| profile.enabled.unwrap_or(true));
            let ipv4 = profile
                .filter(|_| up)
                .and_then(|profile| profile.ipv4.as_ref());
            let ipv6 = profile.and_then(|profile| profile.ipv6.as_ref());
            plans.push(InterfaceConfiguration {
                index: port.index,
                mtu: profile.map(|profile| profile.mtu),
                up,
                address: ipv4.map(|address| address.address),
                prefix_length: ipv4.map_or(0, |address| address.prefix_length),
                route: ipv4
                    .filter(|address| !address.gateway.is_unspecified())
                    .map(|address| DefaultRoute {
                        gateway: address.gateway,
                        protocol: RTPROT_BOOT,
                        attributes: profile.and_then(|profile| profile.default_route).unwrap_or(
                            Attributes {
                                preference: 1,
                                metric: 0,
                            },
                        ),
                    }),
                ipv6: Some(
                    Ipv6Addresses::new(ipv6.map_or(&[][..], |policy| policy.addresses.as_slice()))
                        .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                ),
                delegated: None,
                accept_ra: Some(ipv6.map_or_else(Default::default, |policy| policy.accept_ra)),
                advertise: Some(ipv6.map_or_else(Default::default, |policy| policy.advertise)),
            });
        }
        let transaction = transaction::activate_execution(self.controller.prepare_configuration(
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
                ..Default::default()
            },
        ))
        .map_err(StageError::status)?;
        transaction.confirm().map_err(configuration_status)
    }
}
