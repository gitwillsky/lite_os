//! BGP 域能力由配置 owner 持有；期望值只在原配置树，失败工作由原 Port 推进。
use super::*;
use network_protocol::bgp::{BgpConfigurator, BgpInstance};

impl RouterConfig {
    /// 在调用者持有的配置锁内登记已决定配置的应用工作；禁用时释放域寿命能力。
    pub(super) fn queue_bgp(&self, inner: &mut Inner) {
        if inner
            .saved
            .as_ref()
            .is_some_and(|saved| saved.bgp.is_some())
        {
            if inner.bgp.is_none() {
                inner.bgp_retry_at = Some(native_runtime::system::monotonic());
                self.wake_configuration();
            }
        } else {
            inner.bgp = None;
            inner.bgp_retry_at = None;
        }
    }
    /// 只应用已持久决定的 BGP 配置；初始化握手成功才持有实例，失败保留重试截止。
    pub(super) fn apply_bgp(&self, inner: &mut Inner) -> Result<(), Status> {
        let Some(configuration) = inner.saved.as_ref().and_then(|saved| saved.bgp.as_ref()) else {
            inner.bgp = None;
            inner.bgp_retry_at = None;
            return Ok(());
        };
        if let Some(instance) = inner.bgp.as_ref().filter(|instance| {
            instance.channel().wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        }) {
            let result = instance.update_neighbors(configuration);
            inner.bgp_retry_at = result.as_ref().err().map(|_| transaction::retry_at());
            return result;
        }
        let result = inner
            .link
            .as_ref()
            .ok_or(native_abi::ERR_SHOULD_WAIT)
            .and_then(|link| self.start_bgp(&link.installer, configuration, None));
        match result {
            Ok(instance) => {
                inner.bgp = Some(instance);
                inner.bgp_retry_at = None;
                Ok(())
            }
            Err(error) => {
                inner.bgp_retry_at = Some(transaction::retry_at());
                error!("router-config: BGP application pending: {error}");
                Err(error)
            }
        }
    }
    pub(super) fn start_bgp(
        &self,
        controller: &RoutingConfigurator,
        configuration: &router_protocol::bgp::Configuration,
        scopes: Option<&[u32]>,
    ) -> Result<BgpInstance, Status> {
        let factory =
            BgpConfigurator::new(self.services.connect(service_directory::BGP_CONFIGURATOR)?);
        let minimum = configuration
            .neighbors
            .values()
            .map(|neighbor| neighbor.preference)
            .min()
            .unwrap_or(core::num::NonZeroU32::MIN);
        let source = controller.open_source(network_protocol::routing::Grant {
            protocol: router_protocol::bgp::ROUTE_PROTOCOL,
            ipv6_unicast: configuration
                .neighbors
                .values()
                .any(|neighbor| neighbor.ipv6_unicast),
            minimum_preference: minimum.get(),
        })?;
        let instance = factory.start(configuration, source, minimum, scopes)?;
        instance
            .channel()
            .wait_async(&self.port, KEY_BGP, signals::PEER_CLOSED)?;
        Ok(instance)
    }

    /// 消费域退出通知，复核当前能力后重放唯一持久决定；旧实例的排队通知不影响新域。
    pub(super) fn bgp_closed(&self) {
        let mut inner = self.inner.lock();
        let named = inner.domains.bgp_closed();
        if named {
            self.queue_domains(&mut inner);
        }
        // 固定 key 可残留旧实例的已排队 packet；先核对当前能力，不能因此关闭新实例。
        let default_closed = inner.bgp.as_ref().is_some_and(|instance| {
            instance.channel().wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT)
        });
        if default_closed {
            inner.bgp = None;
            self.queue_bgp(&mut inner);
        }
        if (named || default_closed)
            && let Ok(notification) = Self::prepare_notification(&inner)
        {
            Self::publish_notification(&mut inner, notification);
        }
    }
    /// 向当前配置代际的实际 BGP 实例排队刷新，确认时不改变代际。
    /// # Errors
    /// BGP 未配置、待应用或实例拒绝请求。
    pub(super) fn bgp_refresh(
        &self,
        domain: Option<&str>,
        request: &router_protocol::bgp::RefreshRequest,
    ) -> Result<Revision, Status> {
        let inner = self.inner.lock();
        let instance = bgp_instance(&inner, domain)?.ok_or(native_abi::ERR_NOT_FOUND)?;
        instance.request_refresh(request)?;
        Ok(inner.generation)
    }
    /// 沿同一配置锁查询一个规范前缀；没有 BGP 配置时明确返回空结果。
    /// # Errors
    /// 配置未加载/待应用、实例退出或查询失败。
    pub(super) fn bgp_routes(
        &self,
        domain: Option<&str>,
        query: router_protocol::tracking::IpPrefix,
    ) -> Result<(Revision, router_protocol::bgp::Routes), Status> {
        let inner = self.inner.lock();
        if let Some(instance) = bgp_instance(&inner, domain)? {
            return instance
                .routes(query)
                .map(|routes| (inner.generation, routes));
        }
        Ok((
            inner.generation,
            router_protocol::bgp::Routes {
                query,
                pending: false,
                received: Vec::new(),
                exported: Vec::new(),
                originated: None,
                redistributed: None,
                selected: None,
            },
        ))
    }
    /// 在同一配置锁下返回观察代际与域快照；不把收到的路由当作已安装路由。
    /// # Errors
    /// 无有效配置、尚待应用，或原生域退出/回复非法/资源不足。
    pub(super) fn bgp_status(
        &self,
        domain: Option<&str>,
    ) -> Result<(Revision, router_protocol::bgp::Snapshot), Status> {
        let inner = self.inner.lock();
        if let Some(instance) = bgp_instance(&inner, domain)? {
            return instance
                .snapshot()
                .map(|snapshot| (inner.generation, snapshot));
        }
        Ok((
            inner.generation,
            router_protocol::bgp::Snapshot {
                cluster_id: None,
                neighbors: Vec::new(),
                selected_routes: 0,
                originated_routes: 0,
                redistributed_routes: 0,
                routing_pending: false,
            },
        ))
    }
}

fn bgp_instance<'a>(
    inner: &'a Inner,
    domain: Option<&str>,
) -> Result<Option<&'a BgpInstance>, Status> {
    let saved = inner.saved.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
    if let Some(name) = domain {
        let profile = saved
            .routing_domains
            .get(name)
            .ok_or(native_abi::ERR_NOT_FOUND)?;
        return if profile.bgp.is_some() {
            inner.domains.bgp_instance(name).map(Some)
        } else {
            Ok(None)
        };
    }
    if inner.bgp_retry_at.is_some() || (saved.bgp.is_some() && inner.bgp.is_none()) {
        return Err(native_abi::ERR_SHOULD_WAIT);
    }
    Ok(inner.bgp.as_ref())
}
