use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

use crate::ipc::ReceiveBuffer;
use crate::ipc::{PipeDirection, PipeEnd, PipeRead, PipeWrite};

use super::{
    SocketError, SocketPollState, SocketSendError, SocketType, SocketWaitGuard, SocketWaitSource,
    UnixCredentials,
};

#[path = "unix/datagram_queue.rs"]
mod datagram_queue;
use datagram_queue::DatagramQueue;
#[path = "unix/lifecycle.rs"]
mod lifecycle;
#[path = "unix/namespace.rs"]
mod namespace;
use namespace::NamespaceKey;
pub(crate) use namespace::{UnixAddress, UnixPathIdentity};
#[path = "unix/rights.rs"]
mod rights;
pub(crate) use rights::{UnixNode, UnixPassedFile, UnixRights};
#[path = "unix/rights_graph.rs"]
mod rights_graph;
#[path = "unix/stream.rs"]
mod stream;
use stream::{StreamReceive, StreamTransmit};
#[path = "unix/stream_backlog.rs"]
mod stream_backlog;
use stream_backlog::StreamBacklog;

pub(crate) const SCM_MAX_FD: usize = 253;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;

struct Datagram {
    bytes: Vec<u8>,
    source: Option<UnixAddress>,
    rights: Option<UnixRights>,
}

#[derive(Clone, Copy)]
struct BoundAddress {
    visible: UnixAddress,
    binding: Option<NamespaceKey>,
}

enum SocketState {
    Initial,
    // Connecting 只在 ConnectGuard capability 存活期间存在；Drop 必须回滚到 Initial。
    // 缺失该 transaction state 会让同一 client 的并发 connect 重复分配两套 128 KiB transport。
    Connecting,
    Listening {
        backlog: StreamBacklog<Arc<UnixSocket>>,
    },
    Stream {
        receive: Option<Arc<StreamReceive>>,
        transmit: Option<Arc<StreamTransmit>>,
        peer: Option<UnixAddress>,
        peer_credentials: Option<UnixCredentials>,
    },
    Datagram {
        messages: DatagramQueue<Datagram>,
        peer: Option<Weak<UnixSocket>>,
        peer_credentials: Option<UnixCredentials>,
    },
}

/// @description AF_UNIX endpoint 的连接、监听队列、datagram 边界与地址 owner。
pub(crate) struct UnixSocket {
    // OWNER: node_id 在 backend 构造时唯一分配，pathname/accept/socketpair 都只传递 capability；
    // 缺失稳定 identity 会迫使 SCM graph 以可复用地址或 Arc 指针猜测 node。
    node_id: u64,
    socket_type: SocketType,
    // 创建 AF_UNIX endpoint 时捕获 immutable effective credentials；accepted endpoint
    // 继承 listener identity。缺失该 owner 会迫使 SO_PEERCRED 回调 task 或猜测当前调用者。
    credentials: UnixCredentials,
    // OWNER: state lock uniquely serializes connection/queue/shutdown transitions. Splitting
    // listener queue or stream endpoints would publish readiness inconsistent with I/O state.
    state: Mutex<SocketState>,
    // OWNER: address lock is the endpoint's namespace identity; only bind/drop mutate it and both
    // also update NAMESPACE, preventing a second cached address from drifting.
    address: Mutex<Option<BoundAddress>>,
    // OWNER: OFD constructor 只发布一次 Weak root probe。Weak 避免 Socket↔OFD cycle；若缺失，
    // GC 会把 live descriptor 误判为只有 inflight references 并回收其 receive queue。
    rights_owner: Mutex<Option<Weak<dyn UnixPassedFile>>>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl UnixSocket {
    pub(crate) fn new(
        socket_type: SocketType,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        credentials: UnixCredentials,
        node_id: u64,
    ) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            node_id,
            socket_type,
            credentials,
            state: Mutex::new(match socket_type {
                SocketType::Stream => SocketState::Initial,
                SocketType::Datagram => SocketState::Datagram {
                    messages: DatagramQueue::new(),
                    peer: None,
                    peer_credentials: None,
                },
                SocketType::Raw => unreachable!("AF_UNIX raw type crossed Socket facade"),
            }),
            address: Mutex::new(None),
            rights_owner: Mutex::new(None),
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)
    }

    pub(crate) fn socket_type(&self) -> SocketType {
        self.socket_type
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

    pub(super) fn bind_rights_owner(&self, owner: Weak<dyn UnixPassedFile>) {
        let mut current = self.rights_owner.lock();
        assert!(
            current.is_none(),
            "AF_UNIX socket acquired a second OFD owner"
        );
        *current = Some(owner);
    }

    pub(super) fn externally_rooted(&self, inflight: usize) -> bool {
        self.rights_owner
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_none_or(|owner| owner.externally_referenced(inflight))
    }

    /// @description 清理 GC 已证明不可达 endpoint 中的全部 inflight rights。
    /// @return 无返回值；bytes 可保留，但所有 control capability 在 socket lock 外释放。
    pub(super) fn revoke_rights(&self) {
        let mut state = self.state.lock();
        let receive = match &mut *state {
            SocketState::Stream { receive, .. } => receive.clone(),
            SocketState::Datagram { messages, .. } => {
                let messages = messages.take_all();
                drop(state);
                drop(messages);
                return;
            }
            _ => None,
        };
        drop(state);
        if let Some(receive) = receive {
            receive.revoke_rights();
        }
    }

    pub(crate) fn bind(self: &Arc<Self>, address: UnixAddress) -> Result<(), SocketError> {
        if !address.is_abstract() {
            return Err(SocketError::Invalid);
        }
        self.bind_key(address, NamespaceKey::Abstract(address))
    }

    /// @description 将已由 VFS 创建并授权的 pathname inode 绑定到 endpoint。
    /// @param address 对 getsockname/peername 保留的 canonical pathname。
    /// @param identity VFS socket inode 的稳定 identity。
    /// @return namespace 与 endpoint 原子 publication 成功。
    /// @errors endpoint 已绑定、identity collision 或 OOM 返回明确错误。
    pub(crate) fn bind_path(
        self: &Arc<Self>,
        address: UnixAddress,
        identity: UnixPathIdentity,
    ) -> Result<(), SocketError> {
        if address.is_abstract() {
            return Err(SocketError::Invalid);
        }
        self.bind_key(address, NamespaceKey::Path(identity))
    }

    fn bind_key(
        self: &Arc<Self>,
        address: UnixAddress,
        key: NamespaceKey,
    ) -> Result<(), SocketError> {
        let mut bound = self.address.lock();
        if bound.is_some() {
            return Err(SocketError::Invalid);
        }
        namespace::register(self, key)?;
        *bound = Some(BoundAddress {
            visible: address,
            binding: Some(key),
        });
        Ok(())
    }

    pub(crate) fn address(&self) -> Option<UnixAddress> {
        self.address.lock().map(|address| address.visible)
    }

    pub(crate) fn peer_address(&self) -> Option<UnixAddress> {
        match &*self.state.lock() {
            SocketState::Stream { peer, .. } => *peer,
            _ => None,
        }
    }

    pub(crate) fn lookup(address: &UnixAddress) -> Result<Arc<Self>, SocketError> {
        namespace::lookup(&NamespaceKey::Abstract(*address))
    }

    pub(crate) fn lookup_path(identity: UnixPathIdentity) -> Result<Arc<Self>, SocketError> {
        namespace::lookup(&NamespaceKey::Path(identity))
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        if self.socket_type != SocketType::Stream || self.address.lock().is_none() {
            return Err(SocketError::Invalid);
        }
        let backlog = StreamBacklog::new(backlog);
        let mut state = self.state.lock();
        match &*state {
            SocketState::Initial => {
                *state = SocketState::Listening { backlog };
                Ok(())
            }
            SocketState::Listening { .. } => Ok(()),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        receive_rights: bool,
    ) -> Result<(usize, usize, Option<UnixAddress>, Option<UnixRights>), SocketError> {
        let mut state = self.state.lock();
        match &mut *state {
            SocketState::Stream { receive, .. } => {
                let receive = receive.clone();
                drop(state);
                let Some(receive) = receive else {
                    return Ok((0, 0, None, None));
                };
                match receive.read(output, receive_rights) {
                    (PipeRead::Bytes(count), rights) => Ok((count, count, None, rights)),
                    (PipeRead::Eof, _) => Ok((0, 0, None, None)),
                    (PipeRead::Empty, _) => Err(SocketError::Again),
                }
            }
            SocketState::Datagram { messages, .. } => {
                let (message, became_non_full) = messages.pop().ok_or(SocketError::Again)?;
                drop(state);
                self.consume_notify();
                if became_non_full {
                    self.notify();
                }
                let full_length = message.bytes.len();
                let count = output.append(&message.bytes);
                Ok((count, full_length, message.source, message.rights))
            }
            _ => Err(SocketError::NotConnected),
        }
    }

    pub(crate) fn write_with_rights(
        &self,
        input: &[u8],
        rights: &mut Option<UnixRights>,
    ) -> Result<usize, SocketSendError> {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream { transmit, .. } => {
                let transmit = transmit.clone();
                drop(state);
                let Some(transmit) = transmit else {
                    return Err(SocketError::BrokenPipe.into());
                };
                match transmit
                    .write(input, rights)
                    .map_err(SocketSendError::from)?
                {
                    PipeWrite::Bytes(count) => Ok(count),
                    PipeWrite::Full => Err(SocketSendError::WouldBlock),
                    PipeWrite::Broken => Err(SocketError::BrokenPipe.into()),
                }
            }
            SocketState::Datagram { peer, .. } => {
                let target = peer
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(SocketError::NotConnected)
                    .map_err(SocketSendError::from)?;
                drop(state);
                target.enqueue_datagram(input, self.address(), rights)
            }
            _ => Err(SocketError::NotConnected.into()),
        }
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, SocketSendError> {
        self.write_with_rights(input, &mut None)
    }

    /// @description 发送 bytes，并在 AF_UNIX message/stream barrier 上附着可选 rights。
    /// @param input 本次 byte payload。
    /// @param target datagram 显式目标；None 使用 connected peer。
    /// @param rights 尚未提交的 SCM_RIGHTS；仅在 payload commit 成功后取走。
    /// @return 实际 byte count；失败时 rights 保持归 caller 所有。
    pub(crate) fn send_to_with_rights(
        &self,
        input: &[u8],
        target: Option<&Arc<Self>>,
        rights: &mut Option<UnixRights>,
    ) -> Result<usize, SocketSendError> {
        if self.socket_type == SocketType::Stream {
            return self.write_with_rights(input, rights);
        }
        let target = target
            .cloned()
            .or_else(|| {
                let state = self.state.lock();
                let SocketState::Datagram { peer, .. } = &*state else {
                    return None;
                };
                peer.as_ref().and_then(Weak::upgrade)
            })
            .ok_or(SocketError::NotConnected)
            .map_err(SocketSendError::from)?;
        target.enqueue_datagram(input, self.address(), rights)
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        let state = self.state.lock();
        match &*state {
            SocketState::Initial | SocketState::Connecting => SocketPollState {
                readable: false,
                writable: false,
                hangup: false,
                error: false,
            },
            SocketState::Listening { backlog } => SocketPollState {
                readable: !backlog.is_empty(),
                writable: false,
                hangup: false,
                error: false,
            },
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                let read = receive.as_ref().map(|end| end.poll_state());
                let write = transmit.as_ref().map(|end| end.poll_state());
                SocketPollState {
                    readable: read.is_none_or(|state| state.readable),
                    writable: write.is_some_and(|state| state.writable),
                    hangup: read.is_some_and(|state| state.hangup),
                    error: write.is_some_and(|state| state.error),
                }
            }
            SocketState::Datagram { messages, peer, .. } => {
                let readable = !messages.is_empty();
                let peer = peer.clone();
                drop(state);
                SocketPollState {
                    readable,
                    writable: peer
                        .and_then(|peer| peer.upgrade())
                        .is_none_or(|peer| peer.datagram_capacity_available()),
                    hangup: false,
                    error: false,
                }
            }
        }
    }

    /// @description 投影 socket 所有可能无条件返回或被请求的 poll 状态变化 generation。
    ///
    /// @param events poll interest；stream 的 HUP/ERR 无条件返回，因此仍观察收发两侧。
    /// @return 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                // HUP/ERR 不受 requested mask 限制，因此两侧 generation 都必须参与；否则只关注
                // EPOLLIN 的 ET watcher 会在 peer write-close 时因 generation 未变化而漏掉 HUP。
                let read = receive.as_ref().map_or(0, |end| end.readiness_generation());
                let write = transmit
                    .as_ref()
                    .map_or(0, |end| end.readiness_generation());
                read.max(write)
            }
            SocketState::Datagram { peer, .. } => {
                let peer = (events & POLLOUT != 0).then(|| peer.clone()).flatten();
                drop(state);
                let own = if events & (POLLIN | POLLOUT) != 0 {
                    self.notify_read
                        .pipe()
                        .readiness_generation(PipeDirection::Read)
                } else {
                    0
                };
                peer.and_then(|peer| peer.upgrade()).map_or(own, |peer| {
                    own.max(
                        peer.notify_read
                            .pipe()
                            .readiness_generation(PipeDirection::Read),
                    )
                })
            }
            _ => self
                .notify_read
                .pipe()
                .readiness_generation(PipeDirection::Read),
        }
    }

    /// @description 投影 AF_UNIX wait sources；stream 暴露真实 data Pipe，其余类型暴露内部 edge notification。
    ///
    /// @return 与当前 socket 类型和 endpoint lifecycle 一致的 source 列表。
    pub(in crate::socket) fn wait_sources(
        self: &Arc<Self>,
        events: i16,
    ) -> (super::SocketWaitSources, Option<SocketWaitGuard>) {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let receive = receive.clone();
                let transmit = transmit.clone();
                drop(state);
                (
                    [
                        receive.map(|end| SocketWaitSource::Data {
                            pipe: end.pipe(),
                            direction: PipeDirection::Read,
                        }),
                        transmit.map(|end| SocketWaitSource::Data {
                            pipe: end.pipe(),
                            direction: PipeDirection::Write,
                        }),
                    ],
                    None,
                )
            }
            SocketState::Datagram { peer, .. } => {
                let watches_peer = events & POLLOUT != 0;
                let peer = watches_peer.then(|| peer.clone()).flatten();
                let guard = watches_peer.then(|| SocketWaitGuard::new(self.clone(), peer.clone()));
                drop(state);
                (
                    [
                        (events & (POLLIN | POLLOUT) != 0)
                            .then(|| SocketWaitSource::Notification(self.notify_read.pipe())),
                        peer.and_then(|peer| peer.upgrade())
                            .map(|peer| SocketWaitSource::Notification(peer.notify_read.pipe())),
                    ],
                    guard,
                )
            }
            _ => (
                [
                    Some(SocketWaitSource::Notification(self.notify_read.pipe())),
                    None,
                ],
                None,
            ),
        }
    }

    fn notify(&self) {
        self.notify_write.signal_readiness();
    }

    fn consume_notify(&self) {
        self.notify_read.drain_readiness();
    }

    /// @description 排空 listener/datagram 的内部 readiness edge；stream 的 wait source 是真实 data Pipe，禁止从此消费。
    ///
    /// @return 无返回值；实际 socket readiness 由随后的 level recheck 决定。
    pub(in crate::socket) fn consume_wait_notifications(&self) {
        let peer = match &*self.state.lock() {
            SocketState::Stream { .. } => return,
            SocketState::Datagram { peer, .. } => peer.clone(),
            _ => None,
        };
        self.notify_read.drain_readiness();
        if let Some(peer) = peer.and_then(|peer| peer.upgrade()) {
            peer.notify_read.drain_readiness();
        }
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let mut state = self.state.lock();
        let SocketState::Stream {
            receive, transmit, ..
        } = &mut *state
        else {
            return Err(SocketError::NotConnected);
        };
        let closed_receive = if matches!(how, 0 | 2) {
            receive.take()
        } else {
            None
        };
        let closed_transmit = if matches!(how, 1 | 2) {
            transmit.take()
        } else {
            None
        };
        drop(state);
        drop(closed_receive);
        drop(closed_transmit);
        Ok(())
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        let Some(address) = self.address.get_mut().take() else {
            return;
        };
        if let Some(binding) = address.binding {
            namespace::remove_closed(&binding);
        }
    }
}
