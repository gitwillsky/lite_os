use super::{NetworkStack, now};
use crate::net::net_namespace::{NetworkNamespaceScan, take_network_namespaces};

/// 判断 smoltcp 已发布的下一次协议 soft deadline 是否到期。
///
/// # Returns
///
/// ARP/UDP egress 需要 timer 驱动时返回 `true`；无设备或无 deadline 返回 `false`。
pub(crate) fn network_work_due() -> bool {
    let Some(batch) = take_network_namespaces(NetworkNamespaceScan::Timer) else {
        return true;
    };
    let mut due = batch.backlog;
    let timestamp = now();
    for namespace in batch.namespaces.into_iter().flatten() {
        let Some(stack) = namespace.protocol() else {
            continue;
        };
        let Some(mut network) = stack.try_poll() else {
            // owner或payload loan竞争只回投；不会在idle context等待或丢失域deadline。
            due = true;
            continue;
        };
        due |= network.cleanup_backlog();
        due |= network.route_replies.due(timestamp);
        due |= network.sockets.iter().any(|(_, socket)| {
            matches!(socket, smoltcp::socket::Socket::Tcp(tcp)
                if tcp.maintenance_deadline().is_some_and(|deadline| deadline <= timestamp))
        });
        network.plan_egress_routes();
        let NetworkStack {
            interfaces,
            sockets,
            egress_routes,
            ..
        } = &mut *network;
        due |= interfaces
            .iter_mut()
            .filter(|interface| interface.state.up)
            .any(|interface| {
                let mut routes = egress_routes.iter();
                let index = interface.index;
                interface
                    .interface
                    .poll_at_filtered(timestamp, sockets, |_, _, context| {
                        let route = routes
                            .next()
                            .expect("timer occupied-slot route plan changed");
                        let Some(route) = route.filter(|route| route.interface_index == index)
                        else {
                            return false;
                        };
                        context.set_egress_route(
                            super::ipv4(route.next_hop),
                            route.logical_interface_index,
                            route.kind == super::route::Ipv4RouteKind::Broadcast,
                            Some(route.transport),
                        );
                        true
                    })
                    .is_some_and(|deadline| deadline <= timestamp)
            });
    }
    due
}
