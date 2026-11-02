//! endpoint 的就绪投影与通知：使用者等待就绪 Event，被唤醒后经 [`InetSocket::poll_state`] 重新查询电平状态。

use native_runtime::native_abi::signals;
use network_protocol::SocketPollState;

use super::{InetEndpoint, InetSocket, raw, tcp, udp};

impl InetSocket {
    pub(crate) fn poll_state(&self) -> SocketPollState {
        let Ok(_operation) = self.open_operation() else {
            return SocketPollState::closed();
        };
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return tcp::poll_state(self);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return raw::poll_state(self, handle);
        }
        let Ok(handle) = self.udp_handle() else {
            return SocketPollState::error();
        };
        udp::poll_state(self, handle)
    }

    /// 就绪状态可能已变化：置位就绪 Event（多次置位自然合并）。
    pub(super) fn notify(&self) {
        let _ = self.readiness.signal(0, signals::USER_0);
    }
}
