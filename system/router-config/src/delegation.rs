//! 上联委派会话与下游映射，持久意图只来自当前配置树。

use super::*;
use router_protocol::{
    configuration::PrefixDelegation,
    delegation::{self, DelegatedPrefix},
    interface::DelegatedPrefixes,
    switching::Ports,
};

#[derive(Default)]
pub(super) struct Delegation {
    session: Option<Session>,
    prefixes: Vec<DelegatedPrefix>,
    pub(super) retry_at: Option<i64>,
    // 只有执行事务确认后清除。租约变化或能力失效时缺少这个标记会遗漏下游撤回。
    project_pending: bool,
}

impl Delegation {
    pub(super) fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// Check the session identity against the current upstream and persistent policy.
    pub(super) fn retains(&self, index: u32, config: &PrefixDelegation) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.matches(index, config))
    }

    /// Retire the session and remember whether downstream withdrawal is still owed.
    pub(super) fn retire(&mut self) {
        self.session = None;
        self.project_pending |= !self.prefixes.is_empty();
        self.prefixes.clear();
    }

    /// Retire if this interface was our upstream. Returns true when it matched.
    pub(super) fn remove_interface(&mut self, index: u32) -> bool {
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.interface == index)
        {
            return false;
        }
        self.retire();
        true
    }

    /// Compute current /64 assignments for one downstream subnet in this domain.
    /// # Errors
    /// Memory for the subnet projection could not be reserved.
    pub(super) fn subnets(
        &self,
        config: &PrefixDelegation,
        subnet: u64,
        now: i64,
    ) -> Result<Vec<DelegatedPrefix>, Status> {
        config
            .subnets(&self.prefixes, subnet, now)
            .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    /// Open the configured session after the interface and downstream prefix plan are confirmed.
    /// # Errors
    /// Interface lookup, RPC, observer registration or resource reservation failed.
    pub(super) fn start(
        &mut self,
        owner: &RouterConfig,
        network: &RemoteNamespace,
        index: u32,
        link_changed: &Event,
        config: &PrefixDelegation,
        next_key: &mut u64,
    ) -> Result<(), Status> {
        let snapshot = network
            .interface_snapshot(index)
            .map_err(configuration_status)?;
        let name = snapshot
            .name
            .split(|&byte| byte == 0)
            .next()
            .unwrap_or_default();
        let key = *next_key;
        let next = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        self.session =
            Some(owner.open_delegation(network, index, name, link_changed, config, key)?);
        *next_key = next;
        Ok(())
    }

    /// Decode and apply a lease snapshot or retire its closed session.
    pub(super) fn receive(&mut self, port: &Port, key: u64) -> bool {
        let Some(session) = self.session.as_ref().filter(|session| session.key == key) else {
            return false;
        };
        let received = (|| {
            if session.events.wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT) {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            let message = session.events.read();
            session
                .events
                .wait_async(port, key, signals::READABLE | signals::PEER_CLOSED)?;
            let message = message?;
            if !message.handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            delegation::decode(&message.bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)
        })();
        match received {
            Ok(prefixes) if prefixes != self.prefixes => {
                self.prefixes = prefixes;
                self.project_pending = true;
            }
            Ok(_) | Err(native_abi::ERR_SHOULD_WAIT) => {}
            Err(_) => self.retire(),
        }
        true
    }

    pub(super) fn needs_projection(&self) -> bool {
        self.project_pending
    }

    pub(super) fn confirm_projection(&mut self) {
        self.project_pending = false;
    }
}

struct Session {
    interface: u32,
    duid: [u8; 10],
    iaid: u32,
    accept_reconfigure: bool,
    key: u64,
    events: Channel,
}

impl Session {
    fn matches(&self, index: u32, config: &PrefixDelegation) -> bool {
        self.interface == index
            && self.duid == config.duid()
            && self.iaid == config.iaid
            && self.accept_reconfigure == config.accept_reconfigure
    }
}

pub(super) fn downstream(
    configuration: Option<&Configuration>,
    interface: &Interface,
) -> Option<u64> {
    let configuration = configuration?;
    configuration
        .prefix_delegation
        .as_ref()?
        .downstream
        .iter()
        .find_map(|(name, subnet)| {
            let profile = configuration.interfaces.get(name)?;
            profile
                .device
                .matches(&interface.name, interface.mac)
                .then_some(*subnet)
        })
}

/// Find the configured downstream subnet for a name from the domain's VLAN snapshot.
pub(super) fn vlan_subnet(config: Option<&PrefixDelegation>, name: &[u8; 16]) -> Option<u64> {
    let end = name.iter().position(|&byte| byte == 0)?;
    config?
        .downstream
        .iter()
        .find_map(|(key, &subnet)| (key.as_str().as_bytes() == &name[..end]).then_some(subnet))
}

// DHCP 事件与持久配置共用同一种下游执行行。地址和管理状态随执行接口提供。
pub(super) fn prefix_plan(
    (index, up, address, prefix_length): (u32, bool, Option<Ipv4Addr>, u8),
    prefixes: &[DelegatedPrefix],
    route: Option<router_protocol::interface::DefaultRoute>,
) -> Result<InterfaceConfiguration<'_>, Status> {
    Ok(InterfaceConfiguration {
        index,
        up,
        address,
        prefix_length,
        mtu: None,
        route,
        ipv6: None,
        delegated: Some(
            DelegatedPrefixes::new(prefixes).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
        ),
        accept_ra: None,
        advertise: None,
    })
}

pub(super) fn vlan_assignments<'a>(
    inner: &Inner,
    candidate: Option<&Configuration>,
    ports: Ports<'a>,
    retain: bool,
    now: i64,
) -> Result<
    Vec<(
        router_protocol::switching::Gateway<'a>,
        Vec<DelegatedPrefix>,
    )>,
    Status,
> {
    let intent = candidate.and_then(|config| config.prefix_delegation.as_ref());
    assign_vlan_prefixes(ports, intent, |subnet| {
        RouterConfig::delegated_prefixes(inner, candidate, subnet, now, retain)
    })
}

/// Prepare prefix changes for each resolved VLAN, including withdrawal when its mapping is absent.
/// # Errors
/// Reserving the projection or deriving the prefixes failed.
pub(super) fn assign_vlan_prefixes<'a>(
    ports: Ports<'a>,
    intent: Option<&PrefixDelegation>,
    mut prefixes: impl FnMut(Option<u64>) -> Result<Vec<DelegatedPrefix>, Status>,
) -> Result<
    Vec<(
        router_protocol::switching::Gateway<'a>,
        Vec<DelegatedPrefix>,
    )>,
    Status,
> {
    let mut assignments = Vec::new();
    let gateways = ports
        .gateways()
        .filter(|gateway| gateway.binding.index != 0);
    assignments
        .try_reserve_exact(gateways.clone().count())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for gateway in gateways {
        // 全量重放只知道当前持久决定。无映射也显式清空，确认丢失后不会遗留旧前缀。
        assignments.push((gateway, prefixes(vlan_subnet(intent, &gateway.name))?));
    }
    Ok(assignments)
}

impl RouterConfig {
    fn upstream<'a>(
        &self,
        inner: &'a Inner,
        configuration: &Configuration,
        config: &PrefixDelegation,
    ) -> Result<Option<&'a Interface>, Status> {
        let profile = configuration
            .interfaces
            .get(&config.upstream)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let mut interfaces = inner
            .interfaces
            .iter()
            .filter(|interface| profile.device.matches(&interface.name, interface.mac));
        let Some(interface) = interfaces.next() else {
            return Ok(None);
        };
        if interfaces.next().is_some() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (effective, mtu) = policy::resolve(
            configuration,
            &self.defaults,
            &interface.name,
            interface.mac,
        )
        .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        Ok((effective.method != Method::Disabled && mtu >= 1280).then_some(interface))
    }

    pub(super) fn retains_delegation(
        &self,
        inner: &Inner,
        candidate: Option<&Configuration>,
    ) -> Result<bool, Status> {
        let Some((candidate, config)) = candidate.and_then(|candidate| {
            candidate
                .prefix_delegation
                .as_ref()
                .map(|config| (candidate, config))
        }) else {
            return Ok(false);
        };
        let upstream = self.upstream(inner, candidate, config)?;
        Ok(upstream.is_some_and(|interface| inner.delegation.retains(interface.index, config)))
    }

    pub(super) fn delegated_prefixes(
        inner: &Inner,
        candidate: Option<&Configuration>,
        subnet: Option<u64>,
        now: i64,
        retain: bool,
    ) -> Result<Vec<DelegatedPrefix>, Status> {
        let Some(subnet) = subnet.filter(|_| retain) else {
            return Ok(Vec::new());
        };
        let config = candidate
            .and_then(|configuration| configuration.prefix_delegation.as_ref())
            .expect("downstream requires delegation configuration");
        inner.delegation.subnets(config, subnet, now)
    }

    fn open_delegation(
        &self,
        network: &RemoteNamespace,
        index: u32,
        name: &[u8],
        link_changed: &Event,
        config: &PrefixDelegation,
        key: u64,
    ) -> Result<Session, Status> {
        let channel = self.services.connect(service_directory::DHCP)?;
        let duid = config.duid();
        let mut request = service_rpc::Writer::request(delegation::START);
        request.try_reserve(13 + name.len() + duid.len())?;
        request
            .bytes(name)
            .u32(config.iaid)
            .bytes(&duid)
            .bool(config.accept_reconfigure);
        let mut reply = service_rpc::call(
            &channel,
            &request,
            alloc::vec![
                network.duplicate()?.into_channel().into_handle(),
                link_changed
                    .duplicate(native_abi::rights::SAME)?
                    .into_handle(),
            ],
        )?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let events = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)?;
        Ok(Session {
            interface: index,
            duid,
            iaid: config.iaid,
            accept_reconfigure: config.accept_reconfigure,
            key,
            events,
        })
    }

    pub(super) fn queue_delegation(&self, inner: &mut Inner) {
        if inner.delegation.project_pending
            || inner.delegation.has_session()
            || inner
                .saved
                .as_ref()
                .is_some_and(|saved| saved.prefix_delegation.is_some())
        {
            inner.delegation.retry_at = Some(native_runtime::system::monotonic());
            self.wake_configuration();
        } else {
            inner.delegation.retry_at = None;
        }
    }

    pub(super) fn delegation_interface_removed(&self, inner: &mut Inner, index: u32) {
        if inner.delegation.remove_interface(index) {
            self.queue_delegation(inner);
        }
    }

    /// 处理持久决定或租约更新。启动失败可重试，失效前缀仍由执行层按截止时刻独立退休。
    pub(super) fn apply_delegation(&self, inner: &mut Inner) -> Result<(), Status> {
        let result = self.reconcile_delegation(inner);
        inner.delegation.retry_at = result.err().map(|_| transaction::retry_at());
        if let Err(status) = result {
            error!("router-config: IPv6 prefix delegation pending: {status}");
        }
        result
    }

    fn reconcile_delegation(&self, inner: &mut Inner) -> Result<(), Status> {
        let saved = inner.saved.clone();
        let candidate = saved.as_deref();
        let retain = self.retains_delegation(inner, candidate)?;
        if !retain {
            inner.delegation.retire();
        }
        if inner.delegation.project_pending {
            self.project_delegation(inner, candidate, retain)?;
            inner.delegation.project_pending = false;
        }
        if retain {
            return Ok(());
        }
        let Some((candidate, config)) = candidate.and_then(|candidate| {
            candidate
                .prefix_delegation
                .as_ref()
                .map(|config| (candidate, config))
        }) else {
            return Ok(());
        };
        let Some(interface) = self.upstream(inner, candidate, config)? else {
            return Ok(());
        };
        if interface.retry_at.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let index = interface.index;
        let link_changed = interface.link_changed.duplicate(native_abi::rights::SAME)?;
        let network = &inner
            .link
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .network;
        inner.delegation.start(
            self,
            network,
            index,
            &link_changed,
            config,
            &mut inner.next_key,
        )
    }

    fn project_delegation(
        &self,
        inner: &Inner,
        candidate: Option<&Configuration>,
        retain: bool,
    ) -> Result<(), Status> {
        let Some(policy) = candidate.and_then(|config| config.prefix_delegation.as_ref()) else {
            return Ok(());
        };
        if inner.rollback.is_some() || inner.source_retry_at.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let link = inner.link.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
        let snapshots = link
            .network
            .interface_snapshots()
            .map_err(configuration_status)?;
        let mut assignments = Vec::new();
        assignments
            .try_reserve_exact(snapshots.len().min(policy.downstream.len()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let now = native_runtime::system::monotonic();
        for snapshot in snapshots {
            let (subnet, route) = if snapshot.vlan {
                (
                    vlan_subnet(
                        candidate.and_then(|config| config.prefix_delegation.as_ref()),
                        &snapshot.name,
                    ),
                    None,
                )
            } else {
                let Some(interface) = inner
                    .interfaces
                    .iter()
                    .find(|row| row.index == snapshot.index)
                else {
                    continue;
                };
                let Some(subnet) = downstream(candidate, interface) else {
                    continue;
                };
                if interface.retry_at.is_some() {
                    return Err(native_abi::ERR_SHOULD_WAIT);
                }
                // RIB replaces the entire per-interface default route on every interface
                // transaction, including a lease event that only changes IPv6 prefixes.
                let route = if snapshot.up && snapshot.address.is_some() {
                    let protocol = if interface.config.method == Method::Dhcp {
                        RTPROT_DHCP
                    } else {
                        RTPROT_BOOT
                    };
                    routing::default_route(candidate, interface, interface.gateway, protocol)?
                } else {
                    None
                };
                (Some(subnet), route)
            };
            let Some(subnet) = subnet else { continue };
            let prefixes = Self::delegated_prefixes(
                inner,
                candidate,
                snapshot.up.then_some(subnet),
                now,
                retain,
            )?;
            assignments
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            assignments.push((snapshot, prefixes, route));
        }
        if assignments.is_empty() {
            return Ok(());
        }
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(assignments.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (snapshot, prefixes, route) in &assignments {
            plans.push(prefix_plan(
                (
                    snapshot.index,
                    snapshot.up,
                    snapshot.address,
                    snapshot.prefix_length,
                ),
                prefixes,
                *route,
            )?);
        }
        link.activate_configuration(
            &plans,
            [None, None],
            None,
            None,
            network_protocol::ExecutionPolicy::default(),
        )
        .map_err(StageError::status)?
        .confirm()
        .map_err(configuration_status)
    }

    /// 只有当前会话的事件可发布租约；关闭或无效消息撤销租约。
    pub(super) fn delegation_ready(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        if !inner.delegation.receive(&self.port, key) {
            return self.domain_delegation_ready(&mut inner, key);
        }
        if !inner.delegation.needs_projection() && inner.delegation.has_session() {
            return true;
        }
        if let Ok(notification) = Self::prepare_notification(&inner) {
            let _ = self.apply_delegation(&mut inner);
            Self::publish_notification(&mut inner, notification);
        } else {
            self.queue_delegation(&mut inner);
        }
        true
    }
}
