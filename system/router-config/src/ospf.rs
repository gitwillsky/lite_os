//! OSPF 的期望配置随主事务保存。协议状态由独立进程持有，失败只重放 OSPF 实例。

use super::{
    Configuration, Inner, KEY_OSPF, Revision, RouterConfig, RoutingConfigurator, Status,
    StorageService, native_abi, signals, transaction,
};
use network_protocol::ospf::OspfInstance;
use router_protocol::ospf::{PREFERENCE, ROUTE_PROTOCOL, Session, Snapshot};

pub(super) fn changed(previous: Option<&Configuration>, next: &Configuration) -> bool {
    if previous.and_then(|saved| saved.ospf.as_ref()) != next.ospf.as_ref() {
        return true;
    }
    next.ospf.as_ref().is_some_and(|ospf| {
        previous.is_none_or(|saved| {
            saved.ipv4_forwarding != next.ipv4_forwarding
                || saved.ipv6_forwarding != next.ipv6_forwarding
        }) || ospf.links.keys().any(|name| {
            previous.and_then(|saved| saved.interface_reference(name))
                != next.interface_reference(name)
        })
    })
}

impl RouterConfig {
    fn reserve_ospf_epoch(&self) -> Result<u32, Status> {
        // 与可导入配置独立，恢复旧备份不能回滚 RFC 7166 发送序列。
        const FILE: &[u8] = b"ospf-sequence";
        let last = match self.storage.read(FILE)? {
            Some(bytes) => u32::from_be_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| native_abi::ERR_BAD_STATE)?,
            ),
            None => 0,
        };
        let epoch = last.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        self.storage.write(FILE, &epoch.to_be_bytes())?;
        self.storage.sync()?;
        Ok(epoch)
    }
    pub(super) fn queue_ospf(&self, inner: &mut Inner) {
        if inner
            .saved
            .as_ref()
            .is_some_and(|saved| saved.ospf.is_some())
        {
            if inner.ospf.is_none() && inner.ospf_retry_at.is_none() {
                inner.ospf_retry_at = Some(native_runtime::system::monotonic());
                self.wake_configuration();
            }
        } else {
            inner.ospf = None;
            inner.ospf_retry_at = None;
        }
    }

    pub(super) fn apply_ospf(&self, inner: &mut Inner) -> Result<(), Status> {
        let Some(saved) = inner.saved.as_ref() else {
            return Err(native_abi::ERR_BAD_STATE);
        };
        let Some(configuration) = &saved.ospf else {
            inner.ospf = None;
            inner.ospf_retry_at = None;
            return Ok(());
        };
        let result = (|| {
            let session = configuration
                .project(saved)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let controller = &inner
                .link
                .as_ref()
                .ok_or(native_abi::ERR_SHOULD_WAIT)?
                .installer;
            self.start_ospf(controller, session)
        })();
        match result {
            Ok(instance) => {
                inner.ospf = Some(instance);
                inner.ospf_retry_at = None;
                Ok(())
            }
            Err(error) => {
                inner.ospf_retry_at = Some(transaction::retry_at());
                Err(error)
            }
        }
    }

    pub(super) fn start_ospf(
        &self,
        controller: &RoutingConfigurator,
        mut session: Session,
    ) -> Result<OspfInstance, Status> {
        let factory = self
            .services
            .connect(service_directory::OSPF_CONFIGURATOR)?;
        let source = controller.open_source(network_protocol::routing::Grant {
            protocol: ROUTE_PROTOCOL,
            minimum_preference: PREFERENCE,
            ipv6_unicast: session
                .links
                .values()
                .any(|link| link.settings.v3.is_some()),
        })?;
        if session.needs_authentication_epoch() {
            // 所有域都在配置锁内从同一持久序列租出高位，恢复配置备份不会倒退。
            session.authentication_epoch = Some(self.reserve_ospf_epoch()?);
        }
        let instance = OspfInstance::start(&factory, &session, source)?;
        instance
            .channel()
            .wait_async(&self.port, KEY_OSPF, signals::PEER_CLOSED)?;
        Ok(instance)
    }

    pub(super) fn ospf_closed(&self) {
        let mut inner = self.inner.lock();
        let named = inner.domains.ospf_closed();
        if named {
            self.queue_domains(&mut inner);
        }
        let default_closed = inner
            .ospf
            .as_ref()
            .is_some_and(|instance| instance.channel().wait_one(signals::PEER_CLOSED, 0).is_ok());
        if default_closed {
            inner.ospf = None;
            self.queue_ospf(&mut inner);
        }
        if (named || default_closed)
            && let Ok(notification) = Self::prepare_notification(&inner)
        {
            Self::publish_notification(&mut inner, notification);
        }
    }

    pub(super) fn ospf_status(&self, domain: Option<&str>) -> Result<(Revision, Snapshot), Status> {
        let inner = self.inner.lock();
        let saved = inner.saved.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
        let status = if let Some(name) = domain {
            let profile = saved
                .routing_domains
                .get(name)
                .ok_or(native_abi::ERR_NOT_FOUND)?;
            if profile.ospf.is_some() {
                inner.domains.ospf_instance(name)?.snapshot()?
            } else {
                Snapshot::default()
            }
        } else {
            match (&saved.ospf, &inner.ospf) {
                (None, _) => Snapshot::default(),
                (Some(_), Some(instance)) => instance.snapshot()?,
                (Some(_), None) => return Err(native_abi::ERR_SHOULD_WAIT),
            }
        };
        Ok((inner.generation, status))
    }
}
