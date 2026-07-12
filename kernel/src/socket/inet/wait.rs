use alloc::{sync::Arc, vec, vec::Vec};

use crate::{
    ipc::{Pipe, PipeDirection, PipeRead},
    socket::SocketPollState,
};

use super::{InetEndpoint, InetSocket, raw_endpoint, tcp, udp_endpoint};

impl InetSocket {
    pub(in crate::socket) fn poll_state(&self) -> SocketPollState {
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

    pub(in crate::socket) fn wait_pipes(&self) -> Vec<(Arc<Pipe>, PipeDirection)> {
        vec![(self.notify_read.pipe(), PipeDirection::Read)]
    }

    pub(super) fn notify(&self) {
        if !self.notify_read.pipe().readable() {
            let _ = self.notify_write.write(&[1]);
        }
    }

    pub(super) fn consume_notify(&self) {
        let mut byte = [0];
        if matches!(self.notify_read.read(&mut byte), PipeRead::Bytes(_)) {}
    }
}
