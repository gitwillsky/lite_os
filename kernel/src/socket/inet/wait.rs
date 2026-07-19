use crate::{
    ipc::PipeDirection,
    socket::{SocketPollState, SocketWaitSource},
};

use super::{InetEndpoint, InetSocket, protocol_read, raw_endpoint, tcp, udp_endpoint};

impl InetSocket {
    pub(in crate::socket) fn poll_state(&self) -> SocketPollState {
        let _operation = self.operation.lock();
        let _protocol = protocol_read();
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::poll_state(self);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw_endpoint::poll_state(handle);
        }
        let Ok(handle) = self.udp_handle() else {
            return SocketPollState::error();
        };
        udp_endpoint::poll_state(handle)
    }

    pub(in crate::socket) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    /// @description 把 Internet socket 的内部 edge notification 投影给统一 wait seam。
    ///
    /// @return 单一 source-native read notification source。
    pub(in crate::socket) fn wait_sources(&self) -> crate::socket::SocketWaitSources {
        [
            Some(SocketWaitSource::Notification(self.notify_read.pipe())),
            None,
        ]
    }

    pub(super) fn notify(&self) {
        self.notify_write.signal_readiness();
    }

    pub(super) fn consume_notify(&self) {
        self.consume_wait_notifications();
    }

    /// @description 排空已经观察过的 readiness edge，使下一次 wait registration 不会挂到陈旧 token 上。
    ///
    /// @return 无返回值；并发状态变化要么被随后的 level recheck 看到，要么在注册后再次通知。
    pub(in crate::socket) fn consume_wait_notifications(&self) {
        self.notify_read.drain_readiness();
    }
}
