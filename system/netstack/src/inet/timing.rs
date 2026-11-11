use super::now;
use crate::namespace::{NetworkNamespaceScan, take_network_namespaces};

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
        let Some(network) = stack.try_poll() else {
            // owner或payload loan竞争只回投；不会在idle context等待或丢失域deadline。
            due = true;
            continue;
        };
        due |= network.cleanup_backlog();
        due |= network.interfaces.iter().any(|interface| {
            interface.state.address.is_some() && interface.state.up
                && interface.arp.due(timestamp.total_millis().max(0) as u64)
                || interface.igmp.due(timestamp.total_millis().max(0) as u64)
                || interface
                    .ipv6
                    .as_ref()
                    .is_some_and(|ipv6| ipv6.due(timestamp.total_millis().max(0) as u64))
        });
        due |= network.reassembly.due(timestamp);
        due |= network.identification.due(timestamp);
        due |= network.route_replies.due(timestamp);
        due |= network.udp_due(timestamp);
        due |= network
            .forwarding
            .due(timestamp, network.forwarding_enabled);
        due |= network.tcp_due(timestamp);
        due |= network.raw_due(timestamp);
    }
    due
}
