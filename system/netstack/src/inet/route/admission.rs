//! 单路由与整批 FIB 共用的准入；只读取当前设备/路由事实，不发布节点或分配。
use super::*;

impl RoutingTable {
    pub(super) fn admit(
        &self,
        request: Ipv4RouteRequest,
        interfaces: &[NetworkInterface],
        origin: Origin,
    ) -> Result<Record, Ipv4RouteError> {
        Self::admit_with(
            request,
            interfaces.iter().map(InterfaceFacts::capture),
            origin,
            (interfaces, &[]),
            |destination, bound, minimum_scope| {
                self.lookup_record(
                    Ipv4Addr::UNSPECIFIED,
                    destination,
                    bound,
                    minimum_scope,
                    interfaces.iter().map(InterfaceFacts::capture),
                    |record| origin != Origin::Forwarding || record.origin != Origin::Forwarding,
                )
            },
        )
    }

    /// 单条与复合配置共用字段/设备准入；调用者提供同一候选的事实和查询，不发布任何状态。
    pub(super) fn admit_with(
        mut request: Ipv4RouteRequest,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
        origin: Origin,
        (devices, changes): (
            &[NetworkInterface],
            &[crate::inet::configuration::InterfaceChange],
        ),
        lookup: impl Fn(Ipv4Addr, Option<u32>, u8) -> Option<Record>,
    ) -> Result<Record, Ipv4RouteError> {
        if request.prefix_length > 32
            || request.table == 0
            || request.scope > 254
            || request.kind == RouteKind::Local && request.scope != 254
            || request.kind == RouteKind::Broadcast && request.scope < 253
            || u32::from(request.destination) & mask(request.prefix_length)
                != u32::from(request.destination)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        // 与原生 IPv4 prefsrc 的零值语义一致；不能把未指定源保存成实际 0.0.0.0。
        request.preferred_source = request
            .preferred_source
            .filter(|source| !source.is_unspecified());
        // native ip_metrics_convert先规范化再比较actual alias；超大ADVMSS不是配置错误。
        request.metrics.advertised_mss = request.metrics.advertised_mss.min(65535 - 40);
        let rejecting = matches!(
            request.kind,
            RouteKind::Unreachable | RouteKind::Throw | RouteKind::Blackhole | RouteKind::Prohibit
        );
        let selected = if rejecting {
            if request.gateway.is_some() || request.interface_index.is_some() {
                return Err(Ipv4RouteError::Invalid);
            }
            None
        } else if let Some(IpAddr::V6(gateway)) = request.gateway {
            if request.scope >= 253 || request.kind != RouteKind::Unicast {
                return Err(Ipv4RouteError::Invalid);
            }
            let index = request.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
            super::ipv6::admit_member(
                network_protocol::routes::NextHopMember::<core::net::Ipv6Addr> {
                    interface_index: index,
                    gateway: Some(gateway),
                    on_link: request.on_link,
                    weight: 1,
                },
                devices.iter(),
                changes,
            )
            .map_err(|error| match error {
                SocketError::NoDevice => Ipv4RouteError::NoDevice,
                SocketError::NetworkUnreachable => Ipv4RouteError::NetworkUnreachable,
                _ => Ipv4RouteError::Invalid,
            })?;
            Some(index)
        } else if let Some(IpAddr::V4(gateway)) = request.gateway {
            if request.scope >= 253 {
                return Err(Ipv4RouteError::Invalid);
            }
            if request.on_link {
                let index = request.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
                let interface = interfaces
                    .clone()
                    .find(|interface| interface.index == index && interface.mtu >= 68)
                    .ok_or(Ipv4RouteError::NoDevice)?;
                if !interface.up {
                    return Err(Ipv4RouteError::NetworkDown);
                }
                if gateway.is_unspecified()
                    || gateway.is_multicast()
                    || gateway.is_broadcast()
                    || lookup(gateway, None, 0)
                        .is_some_and(|record| record.request.kind != RouteKind::Unicast)
                {
                    return Err(Ipv4RouteError::Invalid);
                }
                Some(index)
            } else {
                // gateway只能经更窄scope的actual路由到达，不沿另一个default递归伪造邻居。
                let route = lookup(gateway, request.interface_index, 253)
                    .ok_or(Ipv4RouteError::NetworkUnreachable)?;
                if !matches!(route.request.kind, RouteKind::Local | RouteKind::Unicast) {
                    return Err(Ipv4RouteError::Invalid);
                }
                let index = route.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
                let interface = interfaces
                    .clone()
                    .find(|interface| interface.index == index && interface.mtu >= 68)
                    .ok_or(Ipv4RouteError::NoDevice)?;
                if !interface.up {
                    return Err(Ipv4RouteError::NetworkDown);
                }
                Some(index)
            }
        } else {
            if request.on_link {
                return Err(Ipv4RouteError::Invalid);
            }
            let index = request.interface_index.ok_or(Ipv4RouteError::NoDevice)?;
            let interface = interfaces
                .clone()
                .find(|interface| interface.index == index && interface.mtu >= 68)
                .ok_or(Ipv4RouteError::NoDevice)?;
            if request.scope < 254 && !interface.up {
                return Err(Ipv4RouteError::NetworkDown);
            }
            Some(index)
        };
        if let Some(source) = request.preferred_source
            && !source.is_unspecified()
            && !lookup(source, None, 0)
                .is_some_and(|record| record.request.kind == RouteKind::Local)
        {
            return Err(Ipv4RouteError::Invalid);
        }
        Ok(Record {
            next_hop_name: None,
            request,
            origin,
            interface_index: selected,
        })
    }
}
