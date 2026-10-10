//! 实际 packet/listener capacity 和 connected-peer membership 的只读等待投影。
use super::datagram_queue::peer_identity_changed;
use super::{SocketState, UnixSocket};
use crate::socket::{SocketWaitSource, capacity::CapacityKind};
use alloc::sync::{Arc, Weak};
impl UnixSocket {
    pub(in crate::socket) fn capacity_wait_source(&self) -> SocketWaitSource {
        SocketWaitSource::notification(self.notify_read.pipe())
    }

    pub(in crate::socket) fn prepare_capacity_wait(&self) {
        self.notify_read.drain_readiness();
    }

    /// 非原等待状态必须允许重查错误；关闭 race 不得因已排空 notification 永久睡眠。
    pub(in crate::socket) fn capacity_recheck(&self, kind: CapacityKind) -> bool {
        if matches!(kind, CapacityKind::SendMemory) {
            return self.send_memory_available()
                || matches!(
                    &*self.state.lock(),
                    SocketState::Closed
                        | SocketState::Datagram {
                            shutdown: super::PacketShutdown { write: true, .. },
                            ..
                        }
                        | SocketState::Stream { transmit: None, .. }
                );
        }
        match (&*self.state.lock(), kind) {
            (
                SocketState::Datagram {
                    messages,
                    shutdown,
                    peer,
                    ..
                },
                CapacityKind::PacketQueue { sender },
            ) => {
                shutdown.read
                    || peer.is_some()
                    || self.node_id() == sender
                    || !messages.is_full(super::datagram_queue::MAX_DATAGRAMS)
            }
            (
                SocketState::Listening {
                    backlog, shutdown, ..
                },
                CapacityKind::Backlog,
            ) => shutdown.read || backlog.capacity_available(),
            _ => true,
        }
    }

    pub(in crate::socket) fn datagram_capacity_available(&self, sender: u64) -> bool {
        matches!(
            &*self.state.lock(),
            SocketState::Datagram { messages, shutdown, peer, .. } if shutdown.read || peer.is_some() || self.node_id() == sender || !messages.is_full(super::datagram_queue::MAX_DATAGRAMS)
        )
    }

    pub(in crate::socket) fn datagram_peer_changed(
        &self,
        expected: &Option<Weak<UnixSocket>>,
    ) -> bool {
        let state = self.state.lock();
        let SocketState::Datagram { peer, .. } = &*state else {
            return true;
        };
        peer_identity_changed(&peer.as_ref().map(Arc::downgrade), expected)
    }
}
