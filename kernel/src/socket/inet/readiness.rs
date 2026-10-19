use smoltcp::{iface::SocketHandle, socket::udp};

use super::*;
use crate::socket::{SocketPollState, packet};

pub(super) struct PendingNotifications {
    endpoints: [Option<Arc<InetSocket>>; NETWORK_NOTIFICATION_BUDGET],
    length: usize,
    backlog: bool,
}

impl PendingNotifications {
    fn new() -> Self {
        Self {
            endpoints: core::array::from_fn(|_| None),
            length: 0,
            backlog: false,
        }
    }

    fn push(&mut self, endpoint: Arc<InetSocket>) {
        self.endpoints[self.length] = Some(endpoint);
        self.length += 1;
    }

    fn is_full(&self) -> bool {
        self.length == NETWORK_NOTIFICATION_BUDGET
    }

    pub(super) fn backlog(&self) -> bool {
        self.backlog
    }
}

fn became_ready(before: SocketPollState, after: SocketPollState) -> bool {
    !before.readable && after.readable
        || !before.writable && after.writable
        || !before.hangup && after.hangup
        || !before.read_closed && after.read_closed
        || !before.error && after.error
}

/// 从 UDP/raw endpoint map 的首项或严格 cursor 后继开始零分配升序查找。
fn find_endpoint_after<V, R>(
    endpoints: &FallibleMap<SocketHandle, V>,
    after: Option<SocketHandle>,
    mut find: impl FnMut(SocketHandle, &V) -> Option<R>,
) -> Option<R> {
    let mut entries = match after {
        Some(handle) => endpoints.iter_after(&handle),
        None => endpoints.iter(),
    };
    entries.find_map(|(&handle, state)| find(handle, state))
}

impl NetworkStack {
    /// 在协议 poll 前冻结全部 endpoint readiness；缺失该快照会把长期 writable
    /// 误判为每轮新 edge，持续唤醒所有网络 waiter。
    /// 分离借用 SocketSet 与 endpoint AVL，完整快照只走一次 O(N) 中序遍历，
    /// 不逐项重新查树；此阶段不改变树结构，通知 drain 仍按有界 cursor 执行。
    pub(super) fn snapshot_readiness(&mut self) {
        let sockets = &self.sockets;
        self.endpoints.for_each_mut(|handle, state| {
            let socket = sockets.get::<udp::Socket<'static>>(*handle);
            state.readiness = super::udp::poll_state_locked(socket, state);
        });
        self.raw_endpoints.for_each_mut(|handle, state| {
            state.readiness = raw::poll_state_locked(sockets, *handle);
        });
        self.tcp_endpoints.for_each_mut(|_, state| {
            state.readiness = state.poll_state(sockets);
        });
    }

    /// 对照 poll 前快照，只发布 false → true 的 readiness transition。
    pub(super) fn capture_readiness_transitions(&mut self) {
        let sockets = &self.sockets;
        self.endpoints.for_each_mut(|handle, state| {
            let socket = sockets.get::<udp::Socket<'static>>(*handle);
            let after = super::udp::poll_state_locked(socket, state);
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        });
        self.raw_endpoints.for_each_mut(|handle, state| {
            let after = raw::poll_state_locked(sockets, *handle);
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        });
        self.tcp_endpoints.for_each_mut(|_, state| {
            let after = state.poll_state(sockets);
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        });
    }

    fn next_pending_udp(
        &mut self,
        after: Option<SocketHandle>,
    ) -> Option<(SocketHandle, Arc<InetSocket>)> {
        let (handle, endpoint) = find_endpoint_after(&self.endpoints, after, |handle, state| {
            state
                .notification_pending
                .then(|| state.endpoint.upgrade().map(|endpoint| (handle, endpoint)))
                .flatten()
        })?;
        self.endpoints
            .get_mut(&handle)
            .expect("selected UDP endpoint disappeared")
            .notification_pending = false;
        Some((handle, endpoint))
    }

    fn next_pending_raw(
        &mut self,
        after: Option<SocketHandle>,
    ) -> Option<(SocketHandle, Arc<InetSocket>)> {
        let (handle, endpoint) =
            find_endpoint_after(&self.raw_endpoints, after, |handle, state| {
                state
                    .notification_pending
                    .then(|| state.endpoint.upgrade().map(|endpoint| (handle, endpoint)))
                    .flatten()
            })?;
        self.raw_endpoints
            .get_mut(&handle)
            .expect("selected raw endpoint disappeared")
            .notification_pending = false;
        Some((handle, endpoint))
    }

    fn next_pending_tcp(&mut self, after: usize) -> Option<(usize, Arc<InetSocket>)> {
        let (id, endpoint) = self
            .tcp_endpoints
            .iter_after(&after)
            .find_map(|(&id, state)| {
                state
                    .notification_pending
                    .then(|| state.endpoint.upgrade().map(|endpoint| (id, endpoint)))
                    .flatten()
            })?;
        self.tcp_endpoints
            .get_mut(&id)
            .expect("selected TCP endpoint disappeared")
            .notification_pending = false;
        Some((id, endpoint))
    }

    /// 在 exclusive poll owner 内提取固定上限的 endpoint Arc，并清除对应 pending edge。
    pub(super) fn take_pending_notifications(&mut self) -> PendingNotifications {
        let mut pending = PendingNotifications::new();
        let mut udp_cursor = None;
        while !pending.is_full() {
            let Some((handle, endpoint)) = self.next_pending_udp(udp_cursor) else {
                break;
            };
            udp_cursor = Some(handle);
            pending.push(endpoint);
        }
        let mut raw_cursor = None;
        while !pending.is_full() {
            let Some((handle, endpoint)) = self.next_pending_raw(raw_cursor) else {
                break;
            };
            raw_cursor = Some(handle);
            pending.push(endpoint);
        }
        let mut tcp_cursor = 0;
        while !pending.is_full() {
            let Some((id, endpoint)) = self.next_pending_tcp(tcp_cursor) else {
                break;
            };
            tcp_cursor = id;
            pending.push(endpoint);
        }
        // 满批次保守回投一次；下一轮会证明是否仍有 pending，避免为精确计数再扫描全表。
        pending.backlog = pending.is_full();
        pending
    }
}

/// 按稳定 endpoint ID 消费本轮 pending edge，并在 NetworkStack lock 外通知 wait owner。
pub(super) fn notify_pending(pending: PendingNotifications) {
    for endpoint in pending.endpoints.into_iter().flatten() {
        endpoint.notify();
    }
}

/// 全局packet目录每次safe point只drain一批；不随NET域数量重复扫描或无界通知。
pub(super) fn notify_packet_pending() -> bool {
    for _ in 0..NETWORK_NOTIFICATION_BUDGET {
        let Some((_, endpoint)) = packet::take_pending_notification(0) else {
            return false;
        };
        endpoint.notify();
    }
    true
}
