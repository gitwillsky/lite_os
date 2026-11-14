//! DHCP 会话与租约发布；地址变化沿同一配置租约提交，路由属性由当前持久策略提供。
use super::*;

/// Read a lease transition and rearm its one-shot observer on `port` under `key`.
/// # Errors
/// Channel, observer registration or lease decoding failed.
pub(super) fn read_event(
    events: &Channel,
    port: &Port,
    key: u64,
) -> Result<(LeaseEvent, Option<Channel>), Status> {
    let message = events.read();
    if message.is_ok() || matches!(message, Err(native_abi::ERR_SHOULD_WAIT)) {
        events.wait_async(port, key, signals::READABLE | signals::PEER_CLOSED)?;
    }
    let message = message?;
    if message.handles.len() > 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut decoder = Decoder::new(&message.bytes);
    let event = decoder.lease().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    decoder.finish().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    Ok((
        event,
        message.handles.into_iter().next().map(Channel::from_handle),
    ))
}

/// Apply the lease address and default route on `index` in the installer's domain.
/// # Errors
/// The interface transaction could not be prepared, applied or confirmed.
pub(super) fn apply_address(
    installer: &RoutingConfigurator,
    index: u32,
    address: Option<Ipv4Addr>,
    prefix_length: u8,
    route: Option<router_protocol::interface::DefaultRoute>,
) -> Result<(), Status> {
    let plan = InterfaceConfiguration {
        index,
        up: true,
        mtu: None,
        address,
        prefix_length,
        route,
        ipv6: None,
        delegated: None,
        accept_ra: None,
        advertise: None,
    };
    transaction::activate_execution(installer.prepare_configuration(
        &[plan],
        [None, None],
        None,
        None,
        network_protocol::ExecutionPolicy::default(),
    ))
    .map_err(StageError::status)?
    .confirm()
    .map_err(configuration_status)
}

impl RouterConfig {
    /// 先打开新租约能力，不提前关闭旧会话；失败时调用方仍可撤销执行服务变更。
    /// # Errors
    /// 服务不可用、回复非法、资源不足或事件登记失败。
    pub(super) fn open_dhcp(
        &self,
        network: &RemoteNamespace,
        index: u32,
        mac: [u8; 6],
        key: u64,
    ) -> Result<Channel, Status> {
        let dhcp = self.services.connect(service_directory::DHCP)?;
        let mut request = service_rpc::Writer::request(router_protocol::DHCP_V4_START);
        request.try_reserve(14)?;
        request.u32(index).bytes(&mac);
        let network = network.duplicate()?.into_channel();
        let mut reply = service_rpc::call(&dhcp, &request, alloc::vec![network.into_handle()])?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let events = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)?;
        Ok(events)
    }

    fn configure_address(
        link: &Link,
        interface: &mut Interface,
        address: Option<Ipv4Addr>,
        prefix_length: u8,
        route: Option<router_protocol::interface::DefaultRoute>,
    ) -> Result<(), Status> {
        apply_address(
            &link.installer,
            interface.index,
            address,
            prefix_length,
            route,
        )?;
        interface.publish_address(address, prefix_length, route);
        Ok(())
    }

    /// 只处理仍属于当前接口会话的事件；地址和路由在确认后发布，失败交原重试路径。
    pub(super) fn lease_ready(&self, key: u64) {
        let mut inner = self.inner.lock();
        let Some(position) = inner.interfaces.iter().position(|interface| {
            interface
                .dhcp
                .as_ref()
                .is_some_and(|(held, _)| *held == key)
        }) else {
            self.domain_lease_ready(&mut inner, key);
            return;
        };
        let (_, events) = inner.interfaces[position]
            .dhcp
            .as_ref()
            .expect("matched DHCP session");
        let (event, reply) = match read_event(events, &self.port, key) {
            Ok(event) => event,
            // dhcp-client 终止：重新请求（根服务重启它期间连接会等待）。
            Err(status) if status != native_abi::ERR_SHOULD_WAIT => {
                warn!("router-config: dhcp-client session failed ({status}); restarting DHCP");
                // 先退休已关闭能力，否则 network_matches 会把 Some(dead channel) 当成可保留会话，永远不重开 DHCP。
                inner.interfaces[position].dhcp = None;
                inner.interfaces[position].lease = None;
                // 事件路径没有同步请求者；apply 已记录失败并发布实际状态变化。
                let _ = self.apply(&mut inner, position);
                Self::changed(&mut inner);
                return;
            }
            Err(_) => return,
        };
        let Inner {
            link,
            interfaces,
            saved,
            ..
        } = &mut *inner;
        let interface = &mut interfaces[position];
        let Some(link) = link.as_ref() else {
            return;
        };
        let result = match event {
            LeaseEvent::Lost => Self::configure_address(link, interface, None, 0, None),
            LeaseEvent::Bound {
                address,
                prefix_length,
                gateway,
                dns,
                dns_suffix,
                timing,
            } => {
                let same_address = (
                    interface.address,
                    interface.prefix_length,
                    interface.gateway,
                ) == (address, prefix_length, gateway);
                // 续租未改变地址或路由时保留现有配置及连接时长。
                let result = if same_address {
                    Ok(())
                } else {
                    routing::default_route(saved.as_deref(), interface, gateway, RTPROT_DHCP)
                        .and_then(|route| {
                            Self::configure_address(
                                link,
                                interface,
                                Some(address),
                                prefix_length,
                                route,
                            )
                        })
                };
                result.map(|()| {
                    interface.lease = Some(Lease {
                        timing,
                        dns_suffix,
                        dns,
                    });
                })
            }
        };
        if result.is_err() {
            // 自动租约没有同步请求者重试；按当前 DHCP 配置重开会话，避免到下次续租前一直无地址。
            self.queue_application(&mut inner.interfaces[position]);
        }
        Self::changed(&mut inner);
        let status = result.err().unwrap_or(native_abi::OK);
        if status != native_abi::OK {
            error!("router-config: lease configuration failed: {status}");
        }
        if let Some(reply) = reply {
            let _ = reply.write(&status.to_le_bytes(), Vec::new());
        }
    }
}
