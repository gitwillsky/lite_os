use alloc::sync::Weak;

use super::{Socket, SocketBackend, UnixNode, UnixPassedFile};

impl Socket {
    /// 投影 AF_UNIX backend 的稳定 rights-graph node。
    ///
    /// # Returns
    ///
    /// AF_UNIX node；其他 domain 不参与 Unix inflight graph。
    pub(crate) fn unix_node(&self) -> Option<UnixNode> {
        match &self.backend {
            SocketBackend::Unix(socket) => Some(socket.node()),
            _ => None,
        }
    }

    /// 把唯一 OFD Weak probe 绑定到 AF_UNIX backend。
    ///
    /// # Parameters
    ///
    /// - `owner`: 拥有本 Socket facade 的 type-erased OFD Weak capability。
    ///
    /// # Returns
    ///
    /// 无返回值；其他 domain 不参与 Unix inflight graph。
    pub(crate) fn bind_unix_rights_owner(&self, owner: Weak<dyn UnixPassedFile>) {
        if let SocketBackend::Unix(socket) = &self.backend {
            socket.bind_rights_owner(owner);
        }
    }
}
