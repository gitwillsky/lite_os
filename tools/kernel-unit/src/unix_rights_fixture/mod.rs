//! 实际 SCM_RIGHTS/graph/root probe 的 host 装配；只提供 endpoint queue 与 socket façade seam。
use alloc::sync::Arc;
#[derive(Clone, Copy)]
pub(crate) enum SocketIoDirection {
    Receive,
    Send,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NoMemory,
    MessageTooLarge,
    ConnectionRefused,
    TooManyReferences,
}
#[path = "../../../../kernel/src/net/credentials.rs"]
mod credentials;
pub(crate) use credentials::UnixCredentials;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketType {
    Stream,
    SeqPacket,
    Datagram,
    Raw,
}
pub(crate) mod unix;
pub(crate) use unix::{UnixFileLimit, UnixNode, UnixPassedFile, UnixRights, UnixSocket};
pub(crate) struct Socket {
    pub(crate) endpoint: Arc<UnixSocket>,
}
impl Socket {
    pub(crate) fn close_description(&self) {
        self.endpoint.close_description();
    }
    pub(crate) fn unix_node(&self) -> Option<UnixNode> {
        Some(self.endpoint.node())
    }
}

#[path = "unix_rights_limit.rs"]
mod unix_rights_limit;
pub(crate) use unix_rights_limit::ProducedLimit;

#[path = "../../../../kernel/src/net/core_buffer_limit.rs"]
mod core_buffer_limit;
pub(crate) use core_buffer_limit::CoreBufferLimit;
#[path = "../../../../kernel/src/net/core_buffers.rs"]
mod core_buffers;
#[derive(Clone, Copy)]
pub(crate) enum SocketDomain {
    Unix,
    Inet,
    Packet,
    Netlink,
}
