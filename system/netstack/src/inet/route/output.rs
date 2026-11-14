//! 本次主机输出的双族 FIB、选源与路径 MTU；TCP/UDP 共用，不保留第二份路由状态。
use super::super::{NetworkInterface, NetworkStack, SocketError};
use core::net::{IpAddr, Ipv4Addr};
use network_protocol::{
    InetAddress, RouteKind, RouteMetrics,
    routes::{RouteDirection, RouteLookup, RoutePath},
};
use router_protocol::{interface::LOOPBACK_INDEX, policy_routing::Rule};
use ::tcp::time::Instant;

type OutputRoutes<'a> = (&'a super::RoutingTable, &'a super::Ipv6Routes, &'a [Rule]);

pub(in crate::inet) struct Output {
    pub(in crate::inet) interface_index: u32,
    pub(in crate::inet) logical_interface_index: u32,
    pub(in crate::inet) source: IpAddr,
    pub(in crate::inet) neighbor: IpAddr,
    pub(in crate::inet) mtu: usize,
    /// 本机选中出口的默认单播跳数，应用的显式值在各发送入口覆盖它。
    pub(in crate::inet) hop_limit: u8,
    pub(in crate::inet) kind: RouteKind,
    table: u32,
    via_gateway: bool,
    pub(in crate::inet) metrics: RouteMetrics,
    pub(in crate::inet) recovery: Option<(u32, core::net::Ipv6Addr)>,
}
impl NetworkStack {
    pub(in crate::inet) fn has_local_ip(&self, address: IpAddr, interface: Option<u32>) -> bool {
        match address {
            IpAddr::V4(address) => self.has_local_address(address),
            IpAddr::V6(address) => self.interfaces.iter().any(|entry| {
                interface.is_none_or(|index| entry.index == index)
                    && entry.has_ipv6_address(address)
            }),
        }
    }
    /// 从同一 NET 地址/路由与 IP 路径估计形成本次发送事实，不保存 socket 路由副本。
    /// timestamp 是本次操作的冻结单调时刻；poll 传整轮时刻，不能中途重读时钟改变过期裁决。
    /// # Errors
    /// 地址族/接口/源资格或路由非法，返回实际领域错误；不提前创建邻居或占用 TX。
    pub(in crate::inet) fn ip_output(
        &self,
        source: IpAddr,
        destination: InetAddress,
        egress: (Option<u32>, u32),
        timestamp: Instant,
    ) -> Result<Output, SocketError> {
        output_for(
            (
                &self.routing,
                &self.ipv6_routing,
                self.selection_rules.local(),
            ),
            &self.interfaces,
            &self.path_mtu,
            source,
            destination,
            egress,
            timestamp,
        )
    }

    /// 冻结本机输出或指定入接口的选路结果，不推进邻接。
    pub(in crate::inet) fn route_lookup(
        &self,
        query: RouteLookup,
        timestamp: Instant,
    ) -> Result<RoutePath, SocketError> {
        if query.destination.is_multicast() {
            return Err(SocketError::OperationNotSupported);
        }
        let outgoing = match query.direction {
            RouteDirection::Input(index) => {
                return self.input_path(query.source, query.destination, index, query.mark, timestamp);
            }
            RouteDirection::Output(index) => index,
        };
        if !query.source.is_unspecified() && !self.has_local_ip(query.source, outgoing) {
            return Err(SocketError::AddressNotAvailable);
        }
        let output = self.ip_output(
            query.source,
            InetAddress::new(query.destination, 0),
            (outgoing, query.mark),
            timestamp,
        )?;
        Ok(RoutePath {
            table: output.table,
            interface_index: output.interface_index,
            kind: output.kind,
            source: output.source,
            gateway: output.via_gateway.then_some(output.neighbor),
        })
    }
}
/// 同一 NET guard 内分拆借用 FIB、设备与路径估计；让连接/回复队列可独立可变，不创建路线镜像。
pub(in crate::inet) fn output_for(
    routes: OutputRoutes<'_>,
    interfaces: &[super::super::NetworkInterface],
    path_mtu: &super::super::path_mtu::PathMtu,
    source: IpAddr,
    destination: InetAddress,
    egress: (Option<u32>, u32),
    timestamp: Instant,
) -> Result<Output, SocketError> {
    let (_, mark) = egress;
    let initial = output_with_source(
        routes,
        interfaces,
        path_mtu,
        source,
        destination,
        egress,
        timestamp,
    )?;
    // 首次查询负责确定自动源。只有这个源匹配一条本机源网段规则时才按确定源重新选路，
    // 保证首次接纳、点查与后续使用已冻结源的发送一致；不因新出口反复改选源。
    if source.is_unspecified()
        && !initial.source.is_unspecified()
        && initial.kind == RouteKind::Unicast
        && !destination.ip().is_multicast()
        && routes.2.iter().any(|rule| {
            rule.prefix_length != 0 && rule.matches(initial.source, LOOPBACK_INDEX, mark)
        })
    {
        output_with_source(
            routes,
            interfaces,
            path_mtu,
            initial.source,
            destination,
            egress,
            timestamp,
        )
    } else {
        Ok(initial)
    }
}

fn output_with_source(
    routes: OutputRoutes<'_>,
    interfaces: &[super::super::NetworkInterface],
    path_mtu: &super::super::path_mtu::PathMtu,
    source: IpAddr,
    destination: InetAddress,
    egress: (Option<u32>, u32),
    timestamp: Instant,
) -> Result<Output, SocketError> {
    let (outgoing, mark) = egress;
    let interface = |index| {
        NetworkInterface::position(interfaces, index)
            .map(|position| &interfaces[position])
            .ok_or(SocketError::NoDevice)
    };
    if let Some(index) = outgoing {
        interface(index)?;
    }
    if let (IpAddr::V4(source), IpAddr::V4(destination)) = (source, destination.ip().to_canonical())
    {
        let route = routes
            .0
            .select_output(
                source,
                destination,
                outgoing,
                (mark, routes.2),
                super::Ipv4Interfaces::live(interfaces),
            )
            .map_err(super::selection_error)?;
        let chosen = if source.is_unspecified() {
            route
                .preferred_source
                .or_else(|| {
                    interface(route.logical_interface_index)
                        .ok()
                        .and_then(|entry| entry.state.address)
                })
                .unwrap_or(Ipv4Addr::UNSPECIFIED)
        } else {
            source
        };
        if chosen.is_unspecified() && route.next_hop.is_ipv6() {
            return Err(SocketError::AddressNotAvailable);
        }
        return Ok(Output {
            interface_index: route.interface_index,
            logical_interface_index: route.logical_interface_index,
            source: chosen.into(),
            neighbor: route.next_hop,
            mtu: path_mtu.get(
                super::super::path_mtu::Path {
                    interface: route.logical_interface_index,
                    source: chosen.into(),
                    destination: destination.into(),
                },
                route.mtu,
                timestamp.total_millis().max(0) as u64,
            ),
            kind: route.kind,
            table: route.table,
            via_gateway: route.via_gateway,
            hop_limit: super::super::DEFAULT_IPV4_HOP_LIMIT,
            recovery: None,
            metrics: route.metrics,
        });
    }
    let (IpAddr::V6(source), IpAddr::V6(destination)) = (source, destination.ip().to_canonical())
    else {
        return Err(SocketError::NetworkUnreachable);
    };
    if let Some(index) = outgoing {
        interface(index)?;
    }
    if destination.is_unicast_link_local() && outgoing.is_none() {
        return Err(SocketError::Invalid);
    }
    let selected = routes.1.select_output(
        source,
        destination,
        outgoing,
        timestamp.total_millis().max(0) as u64,
        (mark, routes.2),
        super::Ipv6Interfaces::live(interfaces),
    )?;
    let logical = selected.interface;
    let actual = if selected.local { 1 } else { logical };
    let owner = interface(logical)?
        .ipv6
        .as_ref()
        .ok_or(SocketError::NetworkUnreachable)?;
    let chosen = if source.is_unspecified() {
        selected
            .preferred_source
            .or_else(|| owner.source_for(destination))
            .ok_or(SocketError::AddressNotAvailable)?
    } else {
        source
    };
    if chosen.is_loopback() && actual != 1
        || !interfaces.iter().any(|entry| {
            (!chosen.is_unicast_link_local() || entry.index == logical)
                && entry.has_ipv6_address(chosen)
        })
    {
        return Err(SocketError::NetworkUnreachable);
    }
    let entry = interface(actual)?;
    let link_mtu = entry.ipv6_link_mtu();
    if !entry.state.up || link_mtu < 1280 {
        return Err(SocketError::NetworkUnreachable);
    }
    Ok(Output {
        interface_index: actual,
        logical_interface_index: logical,
        source: IpAddr::V6(chosen),
        neighbor: IpAddr::V6(selected.neighbor),
        mtu: path_mtu.get(
            super::super::path_mtu::Path {
                interface: logical,
                source: chosen.into(),
                destination: destination.into(),
            },
            link_mtu,
            timestamp.total_millis().max(0) as u64,
        ),
        kind: if selected.local {
            RouteKind::Local
        } else {
            RouteKind::Unicast
        },
        table: selected.table,
        via_gateway: selected.via_gateway,
        hop_limit: entry
            .ipv6
            .as_ref()
            .map_or(super::super::ipv6::DEFAULT_HOP_LIMIT, |owner| {
                owner.hop_limit()
            }),
        metrics: selected.metrics,
        recovery: selected.recovery,
    })
}

/// Only a submitted packet can trigger recovery of a more preferred IPv6 gateway.
pub(in crate::inet) fn probe_gateway(
    interfaces: &mut [super::super::NetworkInterface],
    recovery: Option<(u32, core::net::Ipv6Addr)>,
    now: Instant,
) {
    if let Some((index, gateway)) = recovery
        && let Ok(position) = interfaces.binary_search_by_key(&index, |entry| entry.index)
        && let Some(owner) = &mut interfaces[position].ipv6
    {
        owner.probe_router(gateway, now.total_millis().max(0) as u64);
    }
}
