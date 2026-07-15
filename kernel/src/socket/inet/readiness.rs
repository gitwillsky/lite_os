use smoltcp::{iface::SocketHandle, socket::udp};

use super::*;
use crate::socket::{SocketPollState, packet};

fn became_ready(before: SocketPollState, after: SocketPollState) -> bool {
    !before.readable && after.readable
        || !before.writable && after.writable
        || !before.hangup && after.hangup
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
    pub(super) fn snapshot_readiness(&mut self) {
        let mut udp_cursor = None;
        while let Some(handle) =
            find_endpoint_after(&self.endpoints, udp_cursor, |handle, _| Some(handle))
        {
            udp_cursor = Some(handle);
            let socket = self.sockets.get::<udp::Socket<'static>>(handle);
            let readiness = SocketPollState {
                readable: socket.can_recv(),
                writable: socket.can_send(),
                hangup: false,
                error: false,
            };
            self.endpoints
                .get_mut(&handle)
                .expect("selected UDP endpoint disappeared")
                .readiness = readiness;
        }
        let mut raw_cursor = None;
        while let Some(handle) =
            find_endpoint_after(&self.raw_endpoints, raw_cursor, |handle, _| Some(handle))
        {
            raw_cursor = Some(handle);
            let readiness = raw_endpoint::poll_state_locked(self, handle);
            self.raw_endpoints
                .get_mut(&handle)
                .expect("selected raw endpoint disappeared")
                .readiness = readiness;
        }
        let mut tcp_cursor = 0;
        while let Some(id) = self
            .tcp_endpoints
            .iter_after(&tcp_cursor)
            .map(|(&id, _)| id)
            .next()
        {
            tcp_cursor = id;
            let readiness = self
                .tcp_endpoints
                .get(&id)
                .expect("selected TCP endpoint disappeared")
                .poll_state(self);
            self.tcp_endpoints
                .get_mut(&id)
                .expect("selected TCP endpoint disappeared")
                .readiness = readiness;
        }
    }

    /// 对照 poll 前快照，只发布 false → true 的 readiness transition。
    pub(super) fn capture_readiness_transitions(&mut self) {
        let mut udp_cursor = None;
        while let Some(handle) =
            find_endpoint_after(&self.endpoints, udp_cursor, |handle, _| Some(handle))
        {
            udp_cursor = Some(handle);
            let socket = self.sockets.get::<udp::Socket<'static>>(handle);
            let after = SocketPollState {
                readable: socket.can_recv(),
                writable: socket.can_send(),
                hangup: false,
                error: false,
            };
            let state = self
                .endpoints
                .get_mut(&handle)
                .expect("selected UDP endpoint disappeared");
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        }
        let mut raw_cursor = None;
        while let Some(handle) =
            find_endpoint_after(&self.raw_endpoints, raw_cursor, |handle, _| Some(handle))
        {
            raw_cursor = Some(handle);
            let after = raw_endpoint::poll_state_locked(self, handle);
            let state = self
                .raw_endpoints
                .get_mut(&handle)
                .expect("selected raw endpoint disappeared");
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        }
        let mut tcp_cursor = 0;
        while let Some(id) = self
            .tcp_endpoints
            .iter_after(&tcp_cursor)
            .map(|(&id, _)| id)
            .next()
        {
            tcp_cursor = id;
            let after = self
                .tcp_endpoints
                .get(&id)
                .expect("selected TCP endpoint disappeared")
                .poll_state(self);
            let state = self
                .tcp_endpoints
                .get_mut(&id)
                .expect("selected TCP endpoint disappeared");
            state.notification_pending |= became_ready(state.readiness, after);
            state.readiness = after;
        }
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
}

/// 按稳定 endpoint ID 消费本轮 pending edge，并在 NetworkStack lock 外通知 wait owner。
pub(super) fn notify_pending(stack: &Mutex<NetworkStack>) {
    let mut udp_cursor = None;
    loop {
        // `while let stack.lock()...` 会把 temporary guard 延长到循环体，令 notify
        // 在 NetworkStack lock 内反向获取 wait registry，和 poll level recheck 死锁。
        let pending = { stack.lock().next_pending_udp(udp_cursor) };
        let Some((handle, endpoint)) = pending else {
            break;
        };
        udp_cursor = Some(handle);
        endpoint.notify();
    }
    let mut raw_cursor = None;
    loop {
        let pending = { stack.lock().next_pending_raw(raw_cursor) };
        let Some((handle, endpoint)) = pending else {
            break;
        };
        raw_cursor = Some(handle);
        endpoint.notify();
    }
    let mut tcp_cursor = 0;
    loop {
        let pending = { stack.lock().next_pending_tcp(tcp_cursor) };
        let Some((id, endpoint)) = pending else {
            break;
        };
        tcp_cursor = id;
        endpoint.notify();
    }
    let mut cursor = 0;
    while let Some((identity, endpoint)) = packet::take_pending_notification(cursor) {
        cursor = identity;
        endpoint.notify();
    }
}
