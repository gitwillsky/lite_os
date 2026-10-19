use super::{SocketError, UnixCredentials};
use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;
#[path = "../../../../kernel/src/socket/unix/rights.rs"]
mod rights;
pub(crate) use rights::{UnixFileLimit, UnixNode, UnixPassedFile, UnixReceivedRights, UnixRights};
#[path = "../../../../kernel/src/socket/unix/connection_backlog.rs"]
mod connection_backlog;
#[path = "../../../../kernel/src/socket/unix/retirement.rs"]
mod retirement;
#[path = "../../../../tools/kernel-view/socket/unix/rights_graph/rights_graph/rights_graph.rs"]
mod rights_graph;
#[path = "../../../../kernel/src/socket/unix/rights_owner.rs"]
mod rights_owner;
#[path = "../../../../kernel/src/socket/unix/send_memory.rs"]
mod send_memory;
use send_memory::SenderCharge;
#[path = "../../../../tools/kernel-view/socket/unix/stream/stream/stream.rs"]
mod stream;
const SCM_MAX_FD: usize = 253;
#[path = "../../../../kernel/src/socket/unix/receive_mode.rs"]
mod receive_mode;
use receive_mode::ReceiveMode;
// 仅生产实际 queued payload/address seam；lifecycle、root 与 backlog 使用生产源码。
struct ProducedMessages(Vec<UnixRights>);
impl ProducedMessages {
    fn take_all(&mut self) -> Vec<UnixRights> {
        core::mem::take(&mut self.0)
    }
}
enum SocketState {
    Closed,
    Datagram {
        messages: ProducedMessages,
        peer: Option<Arc<UnixSocket>>,
    },
    Stream {
        receive: Option<Arc<stream::StreamReceive>>,
        transmit: Option<Arc<stream::StreamTransmit>>,
    },
    Listening {
        backlog: connection_backlog::ConnectionBacklog<Arc<UnixSocket>>,
    },
}
struct BoundAddress {
    binding: Option<usize>,
    path_owner: Option<Arc<dyn Send + Sync>>,
}
mod namespace {
    pub(super) fn remove_closed(_: &usize) {
        unreachable!("rights fixture has no address binding")
    }
}
pub(crate) struct UnixSocket {
    node_id: u64,
    socket_type: super::SocketType,
    send_memory: Mutex<send_memory::SendMemoryState>,
    rights_owner: Mutex<rights_owner::RightsOwner>,
    state: Mutex<SocketState>,
    address: Mutex<Option<BoundAddress>>,
    notify_read: Arc<crate::ipc::PipeEnd>,
    notify_write: Arc<crate::ipc::PipeEnd>,
}
pub(crate) struct StreamChannel {
    sender: alloc::sync::Weak<UnixSocket>,
    receive: Arc<stream::StreamReceive>,
    transmit: Arc<stream::StreamTransmit>,
}
impl StreamChannel {
    // producer 固定给出默认 sender limit，覆盖本 fixture 的实际 small chunks；未提供 memory-full 等待输入。
    fn transport_result(result: stream::StreamWrite) -> crate::ipc::PipeWrite {
        match result {
            stream::StreamWrite::Bytes(count) => crate::ipc::PipeWrite::Bytes(count),
            stream::StreamWrite::Broken => crate::ipc::PipeWrite::Broken,
            stream::StreamWrite::MemoryFull => panic!("rights producer has no memory-full input"),
        }
    }

    // 仅把实际 semantic output 投影成既有 host assertion 类型，不包含 I/O 或 queue 算法。
    fn read_result(result: stream::StreamRead) -> crate::ipc::PipeRead {
        match result {
            stream::StreamRead::Bytes(count) => crate::ipc::PipeRead::Bytes(count),
            stream::StreamRead::Empty => crate::ipc::PipeRead::Empty,
            stream::StreamRead::Eof => crate::ipc::PipeRead::Eof,
        }
    }
    #[cfg(test)]
    pub(crate) fn control_storage_count(&self) -> usize {
        self.receive.control_storage_count_for_test()
    }
    pub(crate) fn generations(&self) -> (u64, u64) {
        (
            self.receive.readiness_generation(),
            self.transmit.readiness_generation(),
        )
    }
    pub(crate) fn close_receive(&self) {
        self.receive.close();
    }
    pub(crate) fn close_transmit(&self) {
        self.transmit.close();
    }

    pub(crate) fn read(
        &self,
        output: &mut crate::ipc::ReceiveBuffer<'_>,
        receive_rights: bool,
    ) -> (crate::ipc::PipeRead, Option<UnixReceivedRights>) {
        self.receive
            .read(
                output,
                if receive_rights {
                    ReceiveMode::Message {
                        rights: true,
                        nonblocking: false,
                    }
                } else {
                    ReceiveMode::Read { nonblocking: false }
                },
                false,
                false,
            )
            .map(|(read, rights, _)| (Self::read_result(read), rights))
            .expect("non-peek read does not allocate")
    }
    pub(crate) fn peek(
        &self,
        output: &mut crate::ipc::ReceiveBuffer<'_>,
        receive_rights: bool,
    ) -> Result<(crate::ipc::PipeRead, Option<UnixReceivedRights>), SocketError> {
        self.receive
            .read(
                output,
                ReceiveMode::Message {
                    rights: receive_rights,
                    nonblocking: false,
                },
                true,
                false,
            )
            .map(|(read, rights, _)| (Self::read_result(read), rights))
    }
    pub(crate) fn read_credentials(
        &self,
        output: &mut crate::ipc::ReceiveBuffer<'_>,
        controls: bool,
        peek: bool,
        pass: bool,
    ) -> Result<
        (
            crate::ipc::PipeRead,
            Option<UnixReceivedRights>,
            Option<UnixCredentials>,
        ),
        SocketError,
    > {
        self.receive
            .read(
                output,
                ReceiveMode::Message {
                    rights: controls,
                    nonblocking: false,
                },
                peek,
                pass,
            )
            .map(|(read, rights, credentials)| (Self::read_result(read), rights, credentials))
    }
    pub(crate) fn write_credentials(
        &self,
        bytes: &[u8],
        rights: &mut Option<UnixRights>,
        credentials: Option<&UnixCredentials>,
    ) -> Result<crate::ipc::PipeWrite, SocketError> {
        let Some(sender) = self.sender.upgrade() else {
            return Ok(crate::ipc::PipeWrite::Broken);
        };
        self.transmit
            .write(&sender, bytes, rights, credentials)
            .map(Self::transport_result)
    }
    pub(crate) fn write(
        &self,
        bytes: &[u8],
        rights: &mut Option<UnixRights>,
    ) -> Result<crate::ipc::PipeWrite, SocketError> {
        let Some(sender) = self.sender.upgrade() else {
            return Ok(crate::ipc::PipeWrite::Broken);
        };
        self.transmit
            .write(&sender, bytes, rights, None)
            .map(Self::transport_result)
    }
}
impl UnixSocket {
    // 该 producer 只装配单向实际 stream channel，没有第二端 peer 输入。
    fn peer_retirement(&self, _: &SocketState, _: bool) -> Option<(Arc<Self>, bool)> {
        None
    }
    fn oriented_peer_closed(&self, _: u64, _: bool) {
        unreachable!("rights producer has no oriented peer input")
    }
    pub(super) fn pass_credentials(&self) -> bool {
        false
    }
    pub(crate) fn new() -> Arc<Self> {
        let (notify_read, notify_write) = crate::ipc::Pipe::notification_pair().unwrap();
        Arc::new(Self {
            node_id: crate::id::next_runtime_object_id(),
            socket_type: super::SocketType::Stream,
            send_memory: Mutex::new(send_memory::SendMemoryState::new()),
            rights_owner: Mutex::new(rights_owner::RightsOwner::Creator),
            state: Mutex::new(SocketState::Datagram {
                messages: ProducedMessages(Vec::new()),
                peer: None,
            }),
            address: Mutex::new(None),
            notify_read,
            notify_write,
        })
    }
    pub(super) fn node_id(&self) -> u64 {
        self.node_id
    }
    pub(super) fn node(self: &Arc<Self>) -> UnixNode {
        UnixNode {
            id: self.node_id,
            socket: Arc::downgrade(self),
        }
    }
    fn notify(&self) {
        self.notify_write.signal_readiness();
    }
    pub(crate) fn close(&self) {
        self.close_description();
    }
    pub(crate) fn closed(&self) -> bool {
        self.is_closed()
    }
    pub(crate) fn listener() -> Arc<Self> {
        let socket = Self::new();
        *socket.state.lock() = SocketState::Listening {
            backlog: connection_backlog::ConnectionBacklog::new(16),
        };
        socket
    }
    pub(crate) fn add_pending(listener: &Arc<Self>, server: &Arc<Self>) {
        let ownership = rights_graph::PreparedBacklog::new(server, listener)
            .unwrap()
            .publish()
            .unwrap();
        server.bind_backlog_owner(ownership);
        let reserved = match &mut *listener.state.lock() {
            SocketState::Listening { backlog } => backlog.reserve().unwrap(),
            _ => unreachable!(),
        };
        let staged = reserved.try_stage(server.clone()).ok().unwrap();
        match &mut *listener.state.lock() {
            SocketState::Listening { backlog } => backlog.commit(staged),
            _ => unreachable!(),
        }
    }
    pub(crate) fn accept_pending(listener: &Arc<Self>) -> Arc<Self> {
        let pending = match &mut *listener.state.lock() {
            SocketState::Listening { backlog } => backlog.pop().unwrap(),
            _ => unreachable!(),
        };
        pending.into_value()
    }
    pub(crate) fn pin_file(&self) -> Option<Arc<dyn UnixPassedFile>> {
        self.pin_description().ok()
    }
    pub(crate) fn bind_file(&self, file: &Arc<crate::file::OpenFileDescription>) {
        let owner: Arc<dyn UnixPassedFile> = file.clone();
        self.bind_rights_owner(Arc::downgrade(&owner));
    }
    pub(crate) fn enqueue(self: &Arc<Self>, mut rights: UnixRights) {
        rights.prepare_attach(self).unwrap();
        let retirement = loop {
            match rights.try_attach(self) {
                Ok(retired) => break retired,
                Err(rights_graph::AttachError::NeedsCollection) => rights.collect().unwrap(),
                Err(rights_graph::AttachError::NeedsPreparation) => {
                    rights.prepare_attach(self).unwrap()
                }
            }
        };
        drop(retirement);
        match &mut *self.state.lock() {
            SocketState::Datagram { messages, .. } => messages.0.push(rights),
            _ => unreachable!(),
        }
        rights_graph::request_collection();
    }
    pub(crate) fn prepare(rights: &mut UnixRights, target: &Arc<Self>) -> Result<(), SocketError> {
        rights.prepare_attach(target)
    }
    pub(crate) fn attach_without_allocation(rights: &mut UnixRights, target: &Arc<Self>) {
        crate::allocation_failure::fail_next();
        let result = rights.try_attach(target);
        let consumed = crate::allocation_failure::was_consumed();
        assert!(!consumed, "prepared graph publication allocated");
        assert!(result.is_ok());
        drop(result.ok().unwrap());
    }
    pub(crate) fn collect(uid: u32, limit: usize) -> Result<(), SocketError> {
        rights_graph::collect(uid, limit)
    }
    pub(crate) fn quota_rejected(rights: &mut UnixRights, target: &Arc<Self>) -> bool {
        matches!(
            rights.try_attach(target),
            Err(rights_graph::AttachError::NeedsCollection)
        )
    }
    pub(crate) fn publish_prepared(rights: &mut UnixRights, target: &Arc<Self>) {
        let retired = rights
            .try_attach(target)
            .ok()
            .expect("prepared graph publication");
        drop(retired);
    }
    pub(crate) fn run_worker(
        clock: impl Fn() -> u64,
        sleep_until: impl FnMut(u64),
        gate: crate::sync::TaskMutexWaitPreparation,
    ) -> ! {
        rights_graph::run_collector(clock, sleep_until, gate)
    }
    pub(crate) fn release_queue(&self) {
        self.revoke_rights();
    }
    pub(crate) fn stream_channel(target: &Arc<Self>) -> StreamChannel {
        let (receive, transmit) = stream::channel(
            crate::ipc::Pipe::notification_pair().unwrap(),
            target,
            target.node_id(),
        )
        .unwrap();
        let retired = core::mem::replace(
            &mut *target.state.lock(),
            SocketState::Stream {
                receive: Some(receive.clone()),
                transmit: None,
            },
        );
        drop(retired);
        StreamChannel {
            sender: Arc::downgrade(target),
            receive,
            transmit,
        }
    }
}

// 本装配仅生产 SCM graph/stream 输入；预算调优必须由完整 endpoint 用例执行，不能返回伪造 floor。
pub(super) fn minimum_buffer(_: crate::socket::SocketIoDirection) -> usize {
    unreachable!("rights-only fixture does not configure endpoint memory limits")
}
