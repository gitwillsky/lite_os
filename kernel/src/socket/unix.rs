use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::ipc::PipeEnd;

use super::{SocketError, SocketSendError, SocketType, UnixCredentials, UnixSendCredentials};

mod capacity;
mod datagram_connect;
mod datagram_disconnect;
mod datagram_queue;
mod packet_send;
mod send_memory;
mod send_preflight;
use send_memory::{SendMemoryState, SenderCharge};
mod readiness;
mod receive;
mod shutdown;
use datagram_queue::DatagramQueue;
mod lifecycle;
mod namespace;
use namespace::{BindingRequest, NamespaceKey};
pub(crate) use namespace::{UnixAddress, UnixPathIdentity};
mod rights;
pub(crate) use rights::{UnixFileLimit, UnixNode, UnixPassedFile, UnixReceivedRights, UnixRights};
mod retirement;
mod rights_graph;
mod rights_owner;
mod stream;
use stream::{StreamReceive, StreamTransmit};
mod connection_backlog;
use connection_backlog::ConnectionBacklog;

pub(crate) const SCM_MAX_FD: usize = 253;

/// native TCP_SKB_MIN_TRUESIZE 公式采用当前真实 packet/frame 元数据 footprint。
pub(super) fn minimum_buffer(direction: super::SocketIoDirection) -> usize {
    let overhead = crate::fallible_tree::VacantEntry::<u64, Datagram>::allocation_size()
        .max(stream::frame_allocation_size());
    let floor = 2048 + overhead;
    match direction {
        super::SocketIoDirection::Send => floor * 2,
        super::SocketIoDirection::Receive => floor,
    }
}

const POLLOUT: i16 = 0x004;

mod receive_mode;
pub(super) use receive_mode::ReceiveMode;

struct Datagram {
    bytes: Vec<u8>,
    // 实际 sender endpoint observer；不保活 OFD，支持发送后 bind/close 的 native 地址查询。
    source: SenderCharge,
    rights: Option<UnixRights>,
    credentials: Option<UnixCredentials>,
}

#[derive(Clone)]
struct BoundAddress {
    visible: UnixAddress,
    binding: Option<NamespaceKey>,
    // 仅保留由 VFS caller 提供的实际 inode lifetime；不开放 inode 操作，不保活 OFD/Task。
    path_owner: Option<Arc<dyn Send + Sync>>,
}

/// Native packet 半关闭状态；只属于实际 endpoint state guard，不复制到 facade 或 poll。
#[derive(Clone, Copy, Default)]
struct PacketShutdown {
    read: bool,
    write: bool,
}
impl PacketShutdown {
    fn apply(&mut self, how: usize) {
        self.read |= matches!(how, 0 | 2);
        self.write |= matches!(how, 1 | 2);
    }
}

enum SocketState {
    /// 实际最后 OFD close 后的 terminal state；Arc 观察者不再保活 transport 或 namespace。
    Closed,
    Initial {
        shutdown: PacketShutdown,
    },
    Listening {
        backlog: ConnectionBacklog<Arc<UnixSocket>>,
        // OWNER: 每次成功 listen 捕获 caller 凭据；connect 在 listener commit guard 内冻结。
        credentials: UnixCredentials,
        shutdown: PacketShutdown,
    },
    Stream {
        receive: Option<Arc<StreamReceive>>,
        transmit: Option<Arc<StreamTransmit>>,
        peer: Arc<UnixSocket>,
        pending_error: Option<SocketError>,
        peer_credentials: Option<UnixCredentials>,
    },
    Datagram {
        messages: DatagramQueue<Datagram>,
        // Native peer 关系保留实际 endpoint；最后 OFD close 消费 state 并断开 Arc cycle。
        peer: Option<Arc<UnixSocket>>,
        peer_credentials: Option<UnixCredentials>,
        shutdown: PacketShutdown,
        pending_error: Option<SocketError>,
    },
}

/// AF_UNIX endpoint 的连接、监听队列、datagram 边界与地址 owner。
pub(crate) struct UnixSocket {
    // 创建/被动连接固定同一实际NET对象；只读引用不随当前Task setns而变化。
    net_namespace: Arc<super::NetNamespace>,
    // OWNER: node_id 在 backend 构造时唯一分配，pathname/accept/socketpair 都只传递 capability；
    // 缺失稳定 identity 会迫使 SCM graph 以可复用地址或 Arc 指针猜测 node。
    node_id: u64,
    // OWNER: 唯一 SO_PASSCRED policy；发送决定是否保存实际凭据，接收决定输出及 stream 边界。
    // 缺失该 flag 会把创建时 peer 身份误作每条消息的发送者，并丢失接收 option 的时序语义。
    pass_credentials: AtomicBool,
    socket_type: SocketType,
    // OWNER: 实际 sender memory 及 buffer limits；message charge 存活即实际 queued/prepared memory。
    // 全部 token retirement 在 endpoint/GRAPH guards 外退款并通知，不复制 queue 费用。
    send_memory: Mutex<SendMemoryState>,
    // OWNER: state lock uniquely serializes connection/queue/shutdown transitions. Splitting
    // listener queue or stream endpoints would publish readiness inconsistent with I/O state.
    state: Mutex<SocketState>,
    // OWNER: address lock is the endpoint's namespace identity; only bind/drop mutate it and both
    // also update NAMESPACE, preventing a second cached address from drifting.
    address: Mutex<Option<BoundAddress>>,
    // OWNER: OFD constructor 只发布一次 Weak root probe。Weak 避免 Socket↔OFD cycle；若缺失，
    // GC 会把 live descriptor 误判为只有 inflight references 并回收其 receive queue。
    rights_owner: Mutex<rights_owner::RightsOwner>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl UnixSocket {
    /// 在唯一 state guard 内观察监听状态；供 Socket 的 SO_ACCEPTCONN 投影。
    pub(super) fn is_listening(&self) -> bool {
        matches!(*self.state.lock(), SocketState::Listening { .. })
    }

    pub(super) fn network_namespace_id(&self) -> u64 {
        self.net_namespace.id()
    }
    pub(crate) fn new(
        socket_type: SocketType,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        node_id: u64,
        net_namespace: Arc<super::NetNamespace>,
    ) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            net_namespace,
            node_id,
            pass_credentials: AtomicBool::new(false),
            socket_type,
            send_memory: Mutex::new(SendMemoryState::new()),
            state: Mutex::new(match socket_type {
                SocketType::Stream => SocketState::Initial {
                    shutdown: PacketShutdown::default(),
                },
                SocketType::Datagram | SocketType::SeqPacket => SocketState::Datagram {
                    messages: DatagramQueue::new(),
                    peer: None,
                    peer_credentials: None,
                    shutdown: PacketShutdown::default(),
                    pending_error: None,
                },
                SocketType::Raw => unreachable!("AF_UNIX raw type crossed Socket facade"),
            }),
            address: Mutex::new(None),
            rights_owner: Mutex::new(rights_owner::RightsOwner::Creator),
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)
    }

    pub(crate) fn set_pass_credentials(&self, enabled: bool) {
        self.pass_credentials.store(enabled, Ordering::Relaxed);
    }
    pub(crate) fn pass_credentials(&self) -> bool {
        self.pass_credentials.load(Ordering::Relaxed)
    }
    /// connect 和 datagram send 的 native autobind 触发点；stream send 不自动绑定。
    pub(crate) fn autobind_if_credentials(self: &Arc<Self>) -> Result<(), SocketError> {
        if self.pass_credentials() {
            namespace::register(
                self,
                BindingRequest::Automatic {
                    seed: self.node_id as u32,
                },
            )?;
        }
        Ok(())
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

    pub(crate) fn bind(self: &Arc<Self>, address: UnixAddress) -> Result<(), SocketError> {
        if address.is_unnamed() {
            return namespace::register(
                self,
                BindingRequest::Automatic {
                    seed: self.node_id as u32,
                },
            );
        }
        if !address.is_abstract() {
            return Err(SocketError::Invalid);
        }
        self.bind_key(
            address,
            NamespaceKey::Abstract(self.network_namespace_id(), address, self.socket_type),
            None,
        )
    }

    /// 将已由 VFS 创建并授权的 pathname inode 绑定到 endpoint。
    ///
    /// # Parameters
    ///
    /// - `address`: 对 getsockname/peername 保留的 canonical pathname。
    /// - `identity`: VFS socket inode 的稳定 identity。
    ///
    /// # Returns
    ///
    /// namespace 与 endpoint 原子 publication 成功。
    ///
    /// # Errors
    ///
    /// endpoint 已绑定、identity collision 或 OOM 返回明确错误。
    pub(crate) fn bind_path(
        self: &Arc<Self>,
        address: UnixAddress,
        identity: UnixPathIdentity,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<(), SocketError> {
        if address.is_abstract() || address.is_unnamed() {
            return Err(SocketError::Invalid);
        }
        self.bind_key(address, NamespaceKey::Path(identity), Some(owner))
    }

    fn bind_key(
        self: &Arc<Self>,
        address: UnixAddress,
        key: NamespaceKey,
        path_owner: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), SocketError> {
        namespace::register(
            self,
            BindingRequest::Named {
                key,
                address,
                path_owner,
            },
        )
    }

    pub(crate) fn address(&self) -> Option<UnixAddress> {
        self.address.lock().as_ref().map(|address| address.visible)
    }

    pub(crate) fn peer_address(&self) -> Result<Option<UnixAddress>, SocketError> {
        let peer = match &*self.state.lock() {
            SocketState::Stream { peer, .. } => Some(peer.clone()),
            SocketState::Datagram { peer, .. } => peer.clone(),
            _ => None,
        };
        peer.map(|peer| peer.address())
            .ok_or(SocketError::NotConnected)
    }

    pub(crate) fn lookup(
        address: &UnixAddress,
        socket_type: SocketType,
        net_namespace: &super::NetNamespace,
    ) -> Result<Arc<Self>, SocketError> {
        namespace::lookup(&NamespaceKey::Abstract(
            net_namespace.id(),
            *address,
            socket_type,
        ))
    }

    pub(crate) fn lookup_path(identity: UnixPathIdentity) -> Result<Arc<Self>, SocketError> {
        namespace::lookup(&NamespaceKey::Path(identity))
    }

    pub(crate) fn listen(
        &self,
        limit: usize,
        captured: UnixCredentials,
    ) -> Result<(), SocketError> {
        if !matches!(self.socket_type, SocketType::Stream | SocketType::SeqPacket)
            || self.address.lock().is_none()
        {
            return Err(SocketError::Invalid);
        }
        let mut state = self.state.lock();
        let retired = match &mut *state {
            SocketState::Initial { shutdown } => {
                let shutdown = *shutdown;
                *state = SocketState::Listening {
                    backlog: ConnectionBacklog::new(limit),
                    credentials: captured,
                    shutdown,
                };
                None
            }
            SocketState::Datagram {
                messages,
                peer,
                shutdown,
                ..
            } if self.socket_type == SocketType::SeqPacket
                && peer.is_none()
                && messages.is_empty() =>
            {
                let shutdown = *shutdown;
                *state = SocketState::Listening {
                    backlog: ConnectionBacklog::new(limit),
                    credentials: captured,
                    shutdown,
                };
                None
            }
            SocketState::Listening {
                backlog,
                credentials,
                ..
            } => {
                backlog.set_limit(limit);
                Some(core::mem::replace(credentials, captured))
            }
            _ => return Err(SocketError::Invalid),
        };
        drop(state);
        drop(retired);
        Ok(())
    }

    pub(crate) fn write_with_ancillary(
        self: &Arc<Self>,
        input: &[u8],
        rights: &mut Option<UnixRights>,
        credentials: &UnixSendCredentials,
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
                    .write(
                        self,
                        input,
                        rights,
                        credentials.for_recipient(
                            self.pass_credentials() || transmit.needs_message_credentials(),
                        ),
                    )
                    .map_err(SocketSendError::from)?
                {
                    stream::StreamWrite::Bytes(count) => Ok(count),
                    stream::StreamWrite::Broken => Err(SocketError::BrokenPipe.into()),
                    stream::StreamWrite::MemoryFull => Err(SocketSendError::Capacity(
                        crate::socket::SocketCapacityWait::new(
                            self.clone(),
                            crate::socket::capacity::CapacityKind::SendMemory,
                        ),
                    )),
                }
            }
            _ => Err(SocketError::NotConnected.into()),
        }
    }

    /// 发送 bytes，并在 AF_UNIX message/stream barrier 上附着可选 rights。
    ///
    /// # Parameters
    ///
    /// - `input`: 本次 byte payload。
    /// - `target`: datagram 显式目标；None 使用 connected peer。
    /// - `rights`: 尚未提交的 SCM_RIGHTS；仅在 payload commit 成功后取走。
    ///
    /// # Returns
    ///
    /// 实际 byte count；失败时 rights 保持归 caller 所有。
    pub(crate) fn send_to_with_ancillary(
        self: &Arc<Self>,
        input: &[u8],
        target: Option<&Arc<Self>>,
        rights: &mut Option<UnixRights>,
        credentials: &UnixSendCredentials,
    ) -> Result<usize, SocketSendError> {
        if self.socket_type == SocketType::Stream {
            return self.write_with_ancillary(input, rights, credentials);
        }
        {
            let state = self.state.lock();
            let SocketState::Datagram { peer, shutdown, .. } = &*state else {
                return Err(SocketError::NotConnected.into());
            };
            if self.socket_type == SocketType::SeqPacket && peer.is_none() {
                return Err(SocketError::NotConnected.into());
            }
            if shutdown.write {
                return Err(SocketError::BrokenPipe.into());
            }
        }
        self.autobind_if_credentials()?;
        let target = if self.socket_type == SocketType::SeqPacket {
            None
        } else {
            target
        };
        let explicit = target.is_some();
        let target = match target {
            Some(target) => target.clone(),
            None => {
                let state = self.state.lock();
                let SocketState::Datagram { peer, .. } = &*state else {
                    return Err(SocketError::NotConnected.into());
                };
                self.connected_datagram_peer(peer)
                    .map_err(SocketSendError::from)?
            }
        };
        let result = target.enqueue_datagram(
            input,
            self,
            rights,
            credentials
                .for_recipient(self.pass_credentials() || target.needs_message_credentials()),
        );
        if matches!(
            result,
            Err(SocketSendError::Error(SocketError::ConnectionRefused))
        ) && target.is_closed()
        {
            return Err(self.retire_dead_datagram_peer(&target, explicit));
        }
        result
    }

    /// 唯一 connected datagram peer resolution；未连接与历史 peer 已关闭的错误不能混用。
    /// Arc 仅保留端点身份；最后 OFD close 仍将其置为 terminal，enqueue 必须复查。
    fn connected_datagram_peer(&self, peer: &Option<Arc<Self>>) -> Result<Arc<Self>, SocketError> {
        peer.clone().ok_or(SocketError::NotConnected)
    }

    fn notify(&self) {
        self.notify_write.signal_readiness();
    }

    fn consume_notify(&self) {
        self.notify_read.drain_readiness();
    }
}

/// 为 composition root 创建的独占 worker 运行实际 SCM collector。
pub(super) fn run_rights_collector(
    clock: impl Fn() -> u64,
    sleep_until: impl FnMut(u64),
    gate: crate::sync::TaskMutexWaitPreparation,
) -> ! {
    rights_graph::run_collector(clock, sleep_until, gate)
}
