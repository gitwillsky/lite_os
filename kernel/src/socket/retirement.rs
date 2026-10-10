//! 唯一 Socket facade 的实际 OFD close；backend observer 只能保活历史 allocation。
use super::{Socket, SocketBackend};

impl Socket {
    /// 消费实际 protocol registration、binding、队列或 transport membership。
    ///
    /// 重复 OFD/未发布 facade cleanup 无副作用；backend 负责在自己的 owner guard 外退休资源。
    pub(crate) fn close_description(&self) {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.close_description(),
            SocketBackend::Inet(socket) => socket.close_description(),
            SocketBackend::Packet(socket) => socket.close_description(),
            SocketBackend::Kobject(socket) => socket.close_description(),
        }
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.close_description();
    }
}
