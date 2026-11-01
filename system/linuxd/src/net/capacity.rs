use alloc::sync::{Arc, Weak};

use super::{SocketError, SocketWaitSource, UnixSocket};

/// 实际 packet queue 或 connection backlog 的等待条件，不保存容量副本。
#[derive(Clone, Copy)]
pub(in crate::net) enum CapacityKind {
    PacketQueue { sender: u64 },
    SendMemory,
    Backlog,
}

/// syscall send/connect 共用的 opaque target-capacity projection。
pub(crate) struct SocketCapacityWait {
    target: Arc<UnixSocket>,
    kind: CapacityKind,
}

impl SocketCapacityWait {
    pub(in crate::net) fn new(target: Arc<UnixSocket>, kind: CapacityKind) -> Self {
        Self { target, kind }
    }

    pub(crate) fn wait_source(&self) -> SocketWaitSource {
        self.target.capacity_wait_source()
    }

    pub(crate) fn prepare_wait(&self) {
        self.target.prepare_capacity_wait();
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.target.capacity_recheck(self.kind)
    }
}

/// 一次 wait-key expansion 捕获的 AF_UNIX datagram peer identity guard。
pub(crate) struct SocketWaitGuard {
    socket: Arc<UnixSocket>,
    peer: Option<Weak<UnixSocket>>,
}

impl SocketWaitGuard {
    pub(in crate::net) fn new(socket: Arc<UnixSocket>, peer: Option<Weak<UnixSocket>>) -> Self {
        Self { socket, peer }
    }

    pub(crate) fn changed(&self) -> bool {
        self.socket.datagram_peer_changed(&self.peer)
    }
}

/// send failure 将普通 source backpressure 与 AF_UNIX target capacity 分离。
pub(crate) enum SocketSendError {
    WouldBlock,
    Capacity(SocketCapacityWait),
    /// 显式目的在 lookup/publication 间关闭；ABI 重新解析实际 pathname/abstract 输入，不等待容量。
    DestinationChanged,
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

/// 连接失败保留实际 listener capacity capability，ABI 层据此阻塞或返回 EAGAIN。
pub(crate) enum SocketConnectError {
    BacklogFull(SocketCapacityWait),
    /// 查找所得 listener/peer 在 publication 前已关闭；必须重新取得实际地址对象。
    DestinationChanged,
    Error(SocketError),
}
impl From<SocketError> for SocketConnectError {
    fn from(error: SocketError) -> Self {
        Self::Error(error)
    }
}
