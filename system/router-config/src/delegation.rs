//! 上联委派会话与下游映射，持久意图只来自当前配置树。

use super::*;
use router_protocol::{
    configuration::PrefixDelegation,
    delegation::{self, DelegatedPrefix},
    interface::DelegatedPrefixes,
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
    fn matches(&self, interface: &Interface, config: &PrefixDelegation) -> bool {
        self.interface == interface.index
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
        Ok(inner
            .delegation
            .session
            .as_ref()
            .zip(upstream)
            .is_some_and(|(session, interface)| session.matches(interface, config)))
    }

    pub(super) fn delegated_prefixes(
        inner: &Inner,
        candidate: Option<&Configuration>,
        interface: &Interface,
        enabled: bool,
        now: i64,
        retain: bool,
    ) -> Result<Vec<DelegatedPrefix>, Status> {
        if !enabled || !retain {
            return Ok(Vec::new());
        }
        let Some(subnet) = downstream(candidate, interface) else {
            return Ok(Vec::new());
        };
        let config = candidate
            .and_then(|configuration| configuration.prefix_delegation.as_ref())
            .expect("downstream requires delegation configuration");
        config
            .subnets(&inner.delegation.prefixes, subnet, now)
            .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    fn open_delegation(
        &self,
        interface: &Interface,
        config: &PrefixDelegation,
        key: u64,
    ) -> Result<Session, Status> {
        let channel = self.services.connect(service_directory::DHCP)?;
        let duid = config.duid();
        let mut request = service_rpc::Writer::request(delegation::START);
        request.try_reserve(13 + interface.name.len() + duid.len())?;
        request
            .bytes(&interface.name)
            .u32(config.iaid)
            .bytes(&duid)
            .bool(config.accept_reconfigure);
        let mut reply = service_rpc::call(
            &channel,
            &request,
            alloc::vec![
                interface
                    .link_changed
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
            interface: interface.index,
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

    fn retire_delegation(inner: &mut Inner) {
        inner.delegation.session = None;
        inner.delegation.project_pending |= !inner.delegation.prefixes.is_empty();
        inner.delegation.prefixes.clear();
    }

    pub(super) fn delegation_interface_removed(&self, inner: &mut Inner, index: u32) {
        if inner
            .delegation
            .session
            .as_ref()
            .is_some_and(|session| session.interface == index)
        {
            Self::retire_delegation(inner);
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
            Self::retire_delegation(inner);
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
        let key = inner.next_key;
        let next = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        inner.delegation.session = Some(self.open_delegation(interface, config, key)?);
        inner.next_key = next;
        Ok(())
    }

    fn project_delegation(
        &self,
        inner: &Inner,
        candidate: Option<&Configuration>,
        retain: bool,
    ) -> Result<(), Status> {
        let count = inner
            .interfaces
            .iter()
            .filter(|interface| downstream(candidate, interface).is_some())
            .count();
        if count == 0 {
            return Ok(());
        }
        let link = inner.link.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
        let mut assignments = Vec::new();
        assignments
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let now = native_runtime::system::monotonic();
        for interface in &inner.interfaces {
            if downstream(candidate, interface).is_some() {
                if interface.retry_at.is_some() {
                    return Err(native_abi::ERR_SHOULD_WAIT);
                }
                let enabled = interface.config.method != Method::Disabled;
                assignments.push((
                    interface,
                    enabled,
                    Self::delegated_prefixes(inner, candidate, interface, enabled, now, retain)?,
                ));
            }
        }
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(count)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for (interface, enabled, prefixes) in &assignments {
            plans.push(InterfaceConfiguration {
                index: interface.index,
                up: *enabled,
                mtu: None,
                address: (!interface.address.is_unspecified()).then_some(interface.address),
                prefix_length: interface.prefix_length,
                route: None,
                ipv6: None,
                delegated: Some(
                    DelegatedPrefixes::new(prefixes).map_err(|_| native_abi::ERR_INVALID_ARGS)?,
                ),
                accept_ra: None,
                advertise: None,
            });
        }
        link.activate_configuration(&plans, [None, None], None, None, network_protocol::ExecutionPolicy::default())
            .map_err(StageError::status)?
            .confirm()
            .map_err(configuration_status)
    }

    /// 只有当前会话的事件可发布租约；关闭或无效消息撤销租约。
    pub(super) fn delegation_ready(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(session) = inner
            .delegation
            .session
            .as_ref()
            .filter(|session| session.key == key)
        else {
            return false;
        };
        let received = (|| {
            if session.events.wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT) {
                return Err(native_abi::ERR_PEER_CLOSED);
            }
            let message = session.events.read()?;
            if !message.handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let prefixes =
                delegation::decode(&message.bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            session
                .events
                .wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)?;
            Ok(prefixes)
        })();
        match received {
            Ok(prefixes) => {
                if prefixes == inner.delegation.prefixes {
                    return true;
                }
                inner.delegation.prefixes = prefixes;
                inner.delegation.project_pending = true;
            }
            Err(_) => Self::retire_delegation(&mut inner),
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
