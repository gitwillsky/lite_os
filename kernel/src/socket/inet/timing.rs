use super::{NETWORK_STACK, NetworkStack, now};

/// @description 判断 smoltcp 已发布的下一次协议 soft deadline 是否到期。
///
/// @return ARP/UDP egress 需要 timer 驱动时返回 `true`；无设备或无 deadline 返回 `false`。
/// @errors 无错误。
pub(crate) fn network_work_due() -> bool {
    let Some(stack) = NETWORK_STACK.get() else {
        return false;
    };
    let mut network = stack.lock();
    let timestamp = now();
    let NetworkStack {
        interface, sockets, ..
    } = &mut *network;
    interface
        .poll_at(timestamp, sockets)
        .is_some_and(|deadline| deadline <= timestamp)
}
