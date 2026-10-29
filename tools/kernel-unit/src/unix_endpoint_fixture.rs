#![allow(
    unused_imports,
    reason = "独立 host fixture 只使用完整 owner 的部分导出，其余接口由其他集成用例覆盖"
)]
//! 实际完整 AF_UNIX owner 装配；facade types/transport factory 是不可变 producer 输入，无 lifecycle 算法副本。
use alloc::sync::Arc;
mod inet {
    // UNIX场景不装配INET；显式未执行类型边界，真实protocol owner由network_endpoint_fixture覆盖。
    pub(crate) struct NetworkStackOwner;
}
#[path = "../../../kernel/src/net/core_buffer_limit.rs"]
mod core_buffer_limit;
pub(crate) use core_buffer_limit::CoreBufferLimit;
#[path = "../../../kernel/src/net/core_buffers.rs"]
mod core_buffers;
#[path = "../../../kernel/src/net/net_namespace.rs"]
mod net_namespace;
pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
pub(crate) fn boot_net() -> Arc<NetNamespace> {
    crate::namespace::boot();
    initialize_net().unwrap();
    initial_net()
}
#[derive(Clone, Copy)]
pub(crate) enum SocketDomain {
    Unix,
    Inet,
    Packet,
    Netlink,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SocketType {
    Stream,
    Datagram,
    SeqPacket,
    Raw,
}
#[derive(Clone, Copy)]
pub(crate) enum SocketIoDirection {
    Receive,
    Send,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NoMemory,
    NoSpace,
    AddressInUse,
    NotConnected,
    AlreadyConnected,
    ConnectionRefused,
    ConnectionReset,
    WrongType,
    Again,
    BrokenPipe,
    TooManyReferences,
    MessageTooLarge,
    OperationNotPermitted,
    OperationNotSupported,
}
pub(crate) struct SocketPollState {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) hangup: bool,
    pub(crate) read_closed: bool,
    pub(crate) error: bool,
}
#[path = "../../../kernel/src/net/wait_source.rs"]
mod wait_source;
pub(crate) use wait_source::SocketWaitSource;
pub(crate) type SocketWaitSources = [Option<SocketWaitSource>; 3];
pub(crate) enum UnixConnectionData {
    Stream {
        first_to_second: (Arc<crate::ipc::PipeEnd>, Arc<crate::ipc::PipeEnd>),
        second_to_first: (Arc<crate::ipc::PipeEnd>, Arc<crate::ipc::PipeEnd>),
    },
    Packet,
}
pub(crate) struct UnixConnectResources {
    pub(crate) server_notify: (Arc<crate::ipc::PipeEnd>, Arc<crate::ipc::PipeEnd>),
    pub(crate) data: UnixConnectionData,
}
#[path = "../../../kernel/src/net/credentials.rs"]
mod credentials;
pub(crate) use credentials::{UnixCredentials, UnixSendCredentials};
#[path = "../../../kernel/src/net/capacity.rs"]
mod capacity;
pub(crate) use capacity::{SocketCapacityWait, SocketSendError, SocketWaitGuard};
#[path = "../../../kernel/src/net/message_limits.rs"]
mod message_limits;
#[path = "../../../tools/kernel-view/net/unix/unix/unix.rs"]
mod unix;
pub(crate) use unix::{
    UnixAddress, UnixFileLimit, UnixNode, UnixPassedFile, UnixPathIdentity, UnixRights, UnixSocket,
};
#[path = "unix_rights_fixture/unix_rights_limit.rs"]
mod unix_file_limit;
pub(crate) use unix_file_limit::ProducedLimit;
pub(crate) struct Socket {
    pub(crate) endpoint: Arc<UnixSocket>,
}
impl Socket {
    pub(crate) fn unix_node(&self) -> Option<UnixNode> {
        Some(self.endpoint.node())
    }
    pub(crate) fn close_description(&self) {
        self.endpoint.close_description();
    }
}
pub(crate) fn bind_file(endpoint: &Arc<UnixSocket>, file: &Arc<crate::file::OpenFileDescription>) {
    let owner: Arc<dyn UnixPassedFile> = file.clone();
    endpoint.bind_rights_owner(Arc::downgrade(&owner));
}
type ReceivedMessage = (
    usize,
    usize,
    Option<UnixAddress>,
    Option<unix::UnixReceivedRights>,
    Option<UnixCredentials>,
);

pub(crate) fn receive(
    endpoint: &UnixSocket,
    output: &mut crate::ipc::ReceiveBuffer<'_>,
    peek: bool,
    rights: bool,
    nonblocking: bool,
) -> Result<ReceivedMessage, SocketError> {
    endpoint.receive(
        output,
        unix::ReceiveMode::Message {
            rights,
            nonblocking,
        },
        peek,
    )
}

/// 显式 OFD 最后引用退休输入；关闭算法来自实际 endpoint owner。
pub(crate) fn close_description(endpoint: &UnixSocket) {
    endpoint.close_description();
}
