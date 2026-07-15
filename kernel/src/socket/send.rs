use alloc::sync::{Arc, Weak};

use super::{SocketError, SocketWaitSource, UnixSocket};

/// @description syscall send orchestration 可等待的 opaque target-capacity projection。
pub(crate) struct SocketSendBlocker {
    target: Arc<UnixSocket>,
}

impl SocketSendBlocker {
    pub(in crate::socket) fn new(target: Arc<UnixSocket>) -> Self {
        Self { target }
    }

    pub(crate) fn wait_source(&self) -> SocketWaitSource {
        self.target.capacity_wait_source()
    }

    pub(crate) fn prepare_wait(&self) {
        self.target.prepare_capacity_wait();
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.target.datagram_capacity_available()
    }
}

/// @description 一次 wait-key expansion 捕获的 AF_UNIX datagram peer identity guard。
pub(crate) struct SocketWaitGuard {
    socket: Arc<UnixSocket>,
    peer: Option<Weak<UnixSocket>>,
}

impl SocketWaitGuard {
    pub(in crate::socket) fn new(socket: Arc<UnixSocket>, peer: Option<Weak<UnixSocket>>) -> Self {
        Self { socket, peer }
    }

    pub(crate) fn changed(&self) -> bool {
        self.socket.datagram_peer_changed(&self.peer)
    }
}

/// @description send failure 将普通 source backpressure 与 AF_UNIX target capacity 分离。
pub(crate) enum SocketSendError {
    WouldBlock,
    PeerFull(SocketSendBlocker),
    Error(SocketError),
}

impl From<SocketError> for SocketSendError {
    fn from(error: SocketError) -> Self {
        match error {
            SocketError::Again => Self::WouldBlock,
            error => Self::Error(error),
        }
    }
}
