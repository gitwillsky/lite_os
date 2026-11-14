//! Reconcile each named domain's upstream and its confirmed downstream prefix projection.

use super::*;
use router_protocol::delegation::DelegatedPrefix;

impl RouterConfig {
    /// Queue execution when a named domain's DHCPv6 session receives a new lease or closes.
    pub(in super::super) fn domain_delegation_ready(&self, inner: &mut Inner, key: u64) -> bool {
        for scope in &mut inner.domains.scopes {
            if scope.delegation.receive(&self.port, key) {
                if scope.delegation.needs_projection() || !scope.delegation.has_session() {
                    scope.delegation.retry_at = Some(native_runtime::system::monotonic());
                    self.queue_domains(inner);
                }
                return true;
            }
        }
        false
    }
}

impl Scope {
    fn upstream(&self, domain: &RoutingDomain) -> Option<&Attached> {
        let intent = domain.prefix_delegation.as_ref()?;
        let profile = domain.interfaces.get(&intent.upstream)?;
        if profile.enabled == Some(false) || profile.mtu < 1280 {
            return None;
        }
        self.interfaces
            .iter()
            .find(|port| profile.device == DeviceMatch::Mac(port.mac))
    }

    pub(super) fn retains_delegation(&self, domain: &RoutingDomain) -> bool {
        self.upstream(domain)
            .zip(domain.prefix_delegation.as_ref())
            .is_some_and(|(port, intent)| self.delegation.retains(port.index, intent))
    }

    pub(super) fn delegated_assignments(
        &self,
        domain: &RoutingDomain,
        retain: bool,
        now: i64,
    ) -> Result<Vec<Vec<DelegatedPrefix>>, Status> {
        let intent = domain.prefix_delegation.as_ref().filter(|_| retain);
        let mut assignments = Vec::new();
        assignments
            .try_reserve_exact(self.interfaces.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for port in &self.interfaces {
            let assignment = intent.and_then(|intent| {
                let subnet = downstream_subnet(domain, intent, port.mac)?;
                port.profile(domain)
                    .filter(|profile| profile.enabled != Some(false))
                    .map(|_| (intent, subnet))
            });
            assignments.push(match assignment {
                Some((intent, subnet)) => self.delegation.subnets(intent, subnet, now)?,
                None => Vec::new(),
            });
        }
        Ok(assignments)
    }

    pub(super) fn start_delegation(
        &mut self,
        owner: &RouterConfig,
        domain: Option<&RoutingDomain>,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let result = (|| {
            if self.delegation.needs_projection() {
                if let Some(domain) = domain {
                    self.project_delegation(domain)?;
                }
                self.delegation.confirm_projection();
            }
            self.open_delegation(owner, domain, next_key)
        })();
        self.delegation.retry_at = result.is_err().then(transaction::retry_at);
        result
    }

    fn project_delegation(&self, domain: &RoutingDomain) -> Result<(), Status> {
        let Some(intent) = &domain.prefix_delegation else {
            return Ok(());
        };
        let now = native_runtime::system::monotonic();
        let retain = self.retains_delegation(domain);
        let assignments = self.delegated_assignments(domain, retain, now)?;
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(intent.downstream.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (port, prefixes) in self.interfaces.iter().zip(&assignments) {
            let Some(profile) = port.profile(domain) else {
                continue;
            };
            if downstream_subnet(domain, intent, port.mac).is_none() {
                continue;
            }
            let snapshot = self
                .network
                .interface_snapshot(port.index)
                .map_err(configuration_status)?;
            let route = if snapshot.up && snapshot.address.is_some() {
                match profile.ipv4.as_ref().map(|ipv4| ipv4.assignment) {
                    Some(Assignment::Static) => profile
                        .ipv4
                        .as_ref()
                        .and_then(|ipv4| domain_default_route(profile, ipv4.gateway, RTPROT_BOOT)),
                    Some(Assignment::Dhcp) => port.lease.as_ref().and_then(|lease| {
                        domain_default_route(profile, lease.gateway, RTPROT_DHCP)
                    }),
                    None => None,
                }
            } else {
                None
            };
            plans.push(delegation::prefix_plan(
                (
                    snapshot.index,
                    snapshot.up,
                    snapshot.address,
                    snapshot.prefix_length,
                ),
                prefixes,
                route,
            )?);
        }
        if plans.is_empty() {
            return Ok(());
        }
        transaction::activate_execution(self.controller.prepare_configuration(
            &plans,
            [None, None],
            None,
            None,
            network_protocol::ExecutionPolicy::default(),
        ))
        .map_err(StageError::status)?
        .confirm()
        .map_err(configuration_status)
    }

    fn open_delegation(
        &mut self,
        owner: &RouterConfig,
        domain: Option<&RoutingDomain>,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let Some(domain) = domain else { return Ok(()) };
        let Some(intent) = domain.prefix_delegation.as_ref() else {
            return Ok(());
        };
        let Some(port) = self.upstream(domain) else {
            return Ok(());
        };
        if self.delegation.retains(port.index, intent) {
            return Ok(());
        }
        let index = port.index;
        let event = port.link_changed.duplicate(native_abi::rights::SAME)?;
        self.delegation
            .start(owner, &self.network, index, &event, intent, next_key)
    }
}

fn downstream_subnet(
    domain: &RoutingDomain,
    intent: &router_protocol::configuration::PrefixDelegation,
    mac: [u8; 6],
) -> Option<u64> {
    intent.downstream.iter().find_map(|(name, &subnet)| {
        let profile = domain.interfaces.get(name)?;
        (profile.device == DeviceMatch::Mac(mac)).then_some(subnet)
    })
}
