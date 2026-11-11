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
        if inner.bgp.as_ref().is_some_and(|instance| {
            instance.channel().wait_one(signals::PEER_CLOSED, 0) == Err(native_abi::ERR_TIMED_OUT)
        }) {
            inner.bgp_retry_at = None;
            return Ok(());
        }
        let start = || {
            let factory =
                BgpConfigurator::new(self.services.connect(service_directory::BGP_CONFIGURATOR)?);
            let minimum = configuration
                .neighbors
                .values()
                .map(|neighbor| neighbor.preference)
                .min()
                .unwrap_or(core::num::NonZeroU32::MIN);
            let source = inner
                .link
                .as_ref()
                .ok_or(native_abi::ERR_SHOULD_WAIT)?
                .installer
                .open_source(network_protocol::routing::Grant {
                    protocol: router_protocol::bgp::ROUTE_PROTOCOL,
                    ipv6_unicast: configuration
                        .neighbors
                        .values()
                        .any(|neighbor| neighbor.ipv6_unicast),
                    minimum_preference: minimum.get(),
                })?;
            let instance = factory.start(configuration, source, minimum)?;
            instance
                .channel()
                .wait_async(&self.port, KEY_BGP, signals::PEER_CLOSED)?;
            Ok::<BgpInstance, Status>(instance)
        };
        match start() {
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
    /// 消费域退出通知，复核当前能力后重放唯一持久决定；旧实例的排队通知不影响新域。
    pub(super) fn bgp_closed(&self) {
        let mut inner = self.inner.lock();
        // 固定 key 可残留旧实例的已排队 packet；先核对当前能力，不能因此关闭新实例。
        if inner.bgp.as_ref().is_some_and(|instance| {
            instance.channel().wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT)
        }) {
            match Self::prepare_notification(&inner) {
                Ok(notification) => {
                    inner.bgp = None;
                    self.queue_bgp(&mut inner);
                    Self::publish_notification(&mut inner, notification);
                }
                Err(_) => {
                    // 外部退出不能回滚；通知资源不足时保留失效能力及真实重放工作，重试不误判为活实例。
                    inner.bgp_retry_at = Some(transaction::retry_at());
                    self.wake_configuration();
                }
            }
        }
    }
    /// 向当前配置代际的实际 BGP 实例排队刷新，确认时不改变代际。
    /// # Errors
    /// BGP 未配置、待应用或实例拒绝请求。
    pub(super) fn bgp_refresh(
        &self,
        request: &router_protocol::bgp::RefreshRequest,
    ) -> Result<u64, Status> {
        let inner = self.inner.lock();
        let saved = inner.saved.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
        if saved.bgp.is_none() {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        if inner.bgp_retry_at.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let instance = inner.bgp.as_ref().ok_or(native_abi::ERR_SHOULD_WAIT)?;
        instance.request_refresh(request)?;
        Ok(inner.generation)
    }
    /// 沿同一配置锁查询一个规范前缀；没有 BGP 配置时明确返回空结果。
    /// # Errors
    /// 配置未加载/待应用、实例退出或查询失败。
    pub(super) fn bgp_routes(
        &self,
        query: router_protocol::tracking::IpPrefix,
    ) -> Result<(u64, router_protocol::bgp::Routes), Status> {
        let inner = self.inner.lock();
        let saved = inner.saved.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
        if inner.bgp_retry_at.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        if let Some(instance) = &inner.bgp {
            return instance
                .routes(query)
                .map(|routes| (inner.generation, routes));
        }
        if saved.bgp.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
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
    pub(super) fn bgp_status(&self) -> Result<(u64, router_protocol::bgp::Snapshot), Status> {
        let inner = self.inner.lock();
        if inner.saved.is_none() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        if inner.bgp_retry_at.is_some() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        if let Some(instance) = &inner.bgp {
            return instance
                .snapshot()
                .map(|snapshot| (inner.generation, snapshot));
        }
        if inner
            .saved
            .as_ref()
            .is_some_and(|saved| saved.bgp.is_some())
        {
            return Err(native_abi::ERR_SHOULD_WAIT);
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
