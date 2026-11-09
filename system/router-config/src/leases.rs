//! DHCP 会话与租约发布；地址变化沿同一配置租约提交，路由属性由当前持久策略提供。
use super::*;

impl RouterConfig {
    /// 先打开新租约能力，不提前关闭旧会话；失败时调用方仍可撤销执行服务变更。
    /// # Errors
    /// 服务不可用、回复非法、资源不足或事件登记失败。
    pub(super) fn open_dhcp(&self, interface: &Interface, key: u64) -> Result<Channel, Status> {
        let dhcp = self.services.connect(service_directory::DHCP)?;
        let mut request = service_rpc::Writer::request(DHCP_START);
        request.try_reserve(14)?;
        request.u32(interface.index).bytes(&interface.mac);
        let mut reply = service_rpc::call(&dhcp, &request, Vec::new())?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let events = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)?;
        Ok(events)
    }

    fn clear_address(link: &Link, interface: &mut Interface) -> Result<(), Status> {
        let plan = Ipv4InterfaceConfiguration {
            index: interface.index,
            up: true,
            mtu: None,
            address: None,
            prefix_length: 0,
            route: None,
        };
        link.activate_configuration(&[plan], None, None)
            .map_err(StageError::status)?
            .confirm()
            .map_err(configuration_status)?;
        interface.publish_address(plan);
        Ok(())
    }

    fn configure_address(
        link: &Link,
        interface: &mut Interface,
        address: Ipv4Addr,
        prefix_length: u8,
        route: Option<router_protocol::interface::DefaultRoute>,
    ) -> Result<(), Status> {
        let plan = Ipv4InterfaceConfiguration {
            index: interface.index,
            up: true,
            mtu: None,
            address: Some(address),
            prefix_length,
            route,
        };
        link.activate_configuration(&[plan], None, None)
            .map_err(StageError::status)?
            .confirm()
            .map_err(configuration_status)?;
        interface.publish_address(plan);
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
            return;
        };
        let message = inner.interfaces[position]
            .dhcp
            .as_ref()
            .map(|(_, events)| events.read());
        let event = match message {
            Some(Ok(message)) if message.handles.len() <= 1 => {
                let mut decoder = Decoder::new(&message.bytes);
                decoder
                    .lease()
                    .ok()
                    .filter(|_| decoder.finish().is_ok())
                    .map(|event| {
                        let reply = message.handles.into_iter().next().map(Channel::from_handle);
                        (event, reply)
                    })
            }
            // dhcp-client 终止：重新请求（根服务重启它期间连接会等待）。
            Some(Err(native_abi::ERR_PEER_CLOSED)) => {
                warn!("router-config: dhcp-client session closed; restarting DHCP");
                // 先退休已关闭能力，否则 network_matches 会把 Some(dead channel) 当成可保留会话，永远不重开 DHCP。
                inner.interfaces[position].dhcp = None;
                inner.interfaces[position].lease = None;
                // 事件路径没有同步请求者；apply 已记录失败并发布实际状态变化。
                let _ = self.apply(&mut inner, position);
                Self::changed(&mut inner);
                return;
            }
            _ => None,
        };
        let Inner {
            link,
            interfaces,
            saved,
            ..
        } = &mut *inner;
        let interface = &mut interfaces[position];
        if let Some((_, events)) = interface.dhcp.as_ref() {
            let _ = events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED);
        }
        let (Some((event, reply)), Some(link)) = (event, link.as_ref()) else {
            return;
        };
        let result = match event {
            LeaseEvent::Lost => Self::clear_address(link, interface),
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
                            Self::configure_address(link, interface, address, prefix_length, route)
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
