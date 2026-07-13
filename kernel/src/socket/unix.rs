use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use spin::{Mutex, Once};

use crate::ipc::{PipeDirection, PipeEnd, PipeRead, PipeWrite};

use super::{SocketError, SocketPollState, SocketType, SocketWaitSource};

const UNIX_PATH_MAX: usize = 108;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixAddress(Vec<u8>);

impl UnixAddress {
    pub(crate) fn new(bytes: &[u8]) -> Result<Self, SocketError> {
        if bytes.is_empty() || bytes.len() > UNIX_PATH_MAX {
            return Err(SocketError::Invalid);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| SocketError::NoMemory)?;
        owned.extend_from_slice(bytes);
        Ok(Self(owned))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

struct Datagram {
    bytes: Vec<u8>,
    source: Option<UnixAddress>,
}

enum SocketState {
    Initial,
    Listening {
        backlog: usize,
        pending: VecDeque<Arc<UnixSocket>>,
    },
    Stream {
        receive: Option<Arc<PipeEnd>>,
        transmit: Option<Arc<PipeEnd>>,
        peer: Option<UnixAddress>,
    },
    Datagram {
        messages: VecDeque<Datagram>,
        peer: Option<Weak<UnixSocket>>,
    },
}

/// @description AF_UNIX endpoint 的连接、监听队列、datagram 边界与地址 owner。
pub(crate) struct UnixSocket {
    socket_type: SocketType,
    // OWNER: state lock uniquely serializes connection/queue/shutdown transitions. Splitting
    // listener queue or stream endpoints would publish readiness inconsistent with I/O state.
    state: Mutex<SocketState>,
    // OWNER: address lock is the endpoint's namespace identity; only bind/drop mutate it and both
    // also update NAMESPACE, preventing a second cached address from drifting.
    address: Mutex<Option<UnixAddress>>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

// OWNER: AF_UNIX module uniquely owns address-to-live-socket resolution. Weak values prevent a
// bound address from keeping a closed OFD alive; bind prunes stale entries before collision checks.
static NAMESPACE: Once<Mutex<BTreeMap<UnixAddress, Weak<UnixSocket>>>> = Once::new();

impl UnixSocket {
    pub(crate) fn new(socket_type: SocketType, notify: (Arc<PipeEnd>, Arc<PipeEnd>)) -> Arc<Self> {
        Arc::new(Self {
            socket_type,
            state: Mutex::new(match socket_type {
                SocketType::Stream => SocketState::Initial,
                SocketType::Datagram => SocketState::Datagram {
                    messages: VecDeque::new(),
                    peer: None,
                },
                SocketType::Raw => unreachable!("AF_UNIX raw type crossed Socket facade"),
            }),
            address: Mutex::new(None),
            notify_read: notify.0,
            notify_write: notify.1,
        })
    }

    pub(crate) fn socket_type(&self) -> SocketType {
        self.socket_type
    }

    pub(crate) fn bind(self: &Arc<Self>, address: UnixAddress) -> Result<(), SocketError> {
        if self.address.lock().is_some() {
            return Err(SocketError::Invalid);
        }
        let mut namespace = NAMESPACE.call_once(|| Mutex::new(BTreeMap::new())).lock();
        namespace.retain(|_, socket| socket.strong_count() != 0);
        if namespace.contains_key(&address) {
            return Err(SocketError::AddressInUse);
        }
        namespace.insert(address.clone(), Arc::downgrade(self));
        *self.address.lock() = Some(address);
        Ok(())
    }

    pub(crate) fn address(&self) -> Option<UnixAddress> {
        self.address.lock().clone()
    }

    pub(crate) fn peer_address(&self) -> Option<UnixAddress> {
        match &*self.state.lock() {
            SocketState::Stream { peer, .. } => peer.clone(),
            _ => None,
        }
    }

    pub(crate) fn lookup(address: &UnixAddress) -> Result<Arc<Self>, SocketError> {
        NAMESPACE
            .call_once(|| Mutex::new(BTreeMap::new()))
            .lock()
            .get(address)
            .and_then(Weak::upgrade)
            .ok_or(SocketError::NotFound)
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        if self.socket_type != SocketType::Stream || self.address.lock().is_none() {
            return Err(SocketError::Invalid);
        }
        let mut state = self.state.lock();
        match &*state {
            SocketState::Initial => {
                *state = SocketState::Listening {
                    backlog: backlog.max(1),
                    pending: VecDeque::new(),
                };
                Ok(())
            }
            SocketState::Listening { .. } => Ok(()),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn connect_stream(
        client: &Arc<Self>,
        listener: &Arc<Self>,
        server: Arc<Self>,
        client_to_server: (Arc<PipeEnd>, Arc<PipeEnd>),
        server_to_client: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<(), SocketError> {
        if client.socket_type != SocketType::Stream || listener.socket_type != SocketType::Stream {
            return Err(SocketError::WrongType);
        }
        let mut client_state = client.state.lock();
        if !matches!(*client_state, SocketState::Initial) {
            return Err(SocketError::AlreadyConnected);
        }
        let mut listener_state = listener.state.lock();
        let SocketState::Listening { backlog, pending } = &mut *listener_state else {
            return Err(SocketError::ConnectionRefused);
        };
        if pending.len() >= *backlog {
            return Err(SocketError::Again);
        }
        pending.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
        *client_state = SocketState::Stream {
            receive: Some(server_to_client.0),
            transmit: Some(client_to_server.1),
            peer: listener.address(),
        };
        *server.state.lock() = SocketState::Stream {
            receive: Some(client_to_server.0),
            transmit: Some(server_to_client.1),
            peer: client.address(),
        };
        *server.address.lock() = listener.address();
        pending.push_back(server);
        drop(listener_state);
        drop(client_state);
        listener.notify();
        Ok(())
    }

    pub(crate) fn pair(
        first: &Arc<Self>,
        second: &Arc<Self>,
        first_to_second: (Arc<PipeEnd>, Arc<PipeEnd>),
        second_to_first: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<(), SocketError> {
        match first.socket_type {
            SocketType::Stream => {
                *first.state.lock() = SocketState::Stream {
                    receive: Some(second_to_first.0),
                    transmit: Some(first_to_second.1),
                    peer: None,
                };
                *second.state.lock() = SocketState::Stream {
                    receive: Some(first_to_second.0),
                    transmit: Some(second_to_first.1),
                    peer: None,
                };
            }
            SocketType::Datagram => {
                if let SocketState::Datagram { peer, .. } = &mut *first.state.lock() {
                    *peer = Some(Arc::downgrade(second));
                }
                if let SocketState::Datagram { peer, .. } = &mut *second.state.lock() {
                    *peer = Some(Arc::downgrade(first));
                }
            }
            SocketType::Raw => unreachable!("AF_UNIX raw pair crossed Socket facade"),
        }
        Ok(())
    }

    pub(crate) fn connect_datagram(&self, peer_socket: &Arc<Self>) -> Result<(), SocketError> {
        if self.socket_type != SocketType::Datagram
            || peer_socket.socket_type != SocketType::Datagram
        {
            return Err(SocketError::WrongType);
        }
        let mut state = self.state.lock();
        let SocketState::Datagram { peer, .. } = &mut *state else {
            return Err(SocketError::WrongType);
        };
        *peer = Some(Arc::downgrade(peer_socket));
        Ok(())
    }

    pub(crate) fn accept(&self) -> Result<Arc<Self>, SocketError> {
        let mut state = self.state.lock();
        let SocketState::Listening { pending, .. } = &mut *state else {
            return Err(SocketError::Invalid);
        };
        let accepted = pending.pop_front().ok_or(SocketError::Again)?;
        drop(state);
        self.consume_notify();
        Ok(accepted)
    }

    pub(crate) fn receive(
        &self,
        output: &mut [u8],
    ) -> Result<(usize, Option<UnixAddress>), SocketError> {
        let mut state = self.state.lock();
        match &mut *state {
            SocketState::Stream { receive, .. } => {
                let receive = receive.clone();
                drop(state);
                let Some(receive) = receive else {
                    return Ok((0, None));
                };
                match receive.read(output) {
                    PipeRead::Bytes(count) => Ok((count, None)),
                    PipeRead::Eof => Ok((0, None)),
                    PipeRead::Empty => Err(SocketError::Again),
                }
            }
            SocketState::Datagram { messages, .. } => {
                let message = messages.pop_front().ok_or(SocketError::Again)?;
                drop(state);
                self.consume_notify();
                let count = output.len().min(message.bytes.len());
                output[..count].copy_from_slice(&message.bytes[..count]);
                Ok((count, message.source))
            }
            _ => Err(SocketError::NotConnected),
        }
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, SocketError> {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream { transmit, .. } => {
                let transmit = transmit.clone();
                drop(state);
                let Some(transmit) = transmit else {
                    return Err(SocketError::BrokenPipe);
                };
                match transmit.write_stream(input) {
                    PipeWrite::Bytes(count) => Ok(count),
                    PipeWrite::Full => Err(SocketError::Again),
                    PipeWrite::Broken => Err(SocketError::BrokenPipe),
                }
            }
            SocketState::Datagram { peer, .. } => {
                let target = peer
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .ok_or(SocketError::NotConnected)?;
                drop(state);
                target.enqueue_datagram(input, self.address())
            }
            _ => Err(SocketError::NotConnected),
        }
    }

    pub(crate) fn send_to(
        &self,
        input: &[u8],
        target: Option<&Arc<Self>>,
    ) -> Result<usize, SocketError> {
        if self.socket_type == SocketType::Stream {
            return self.write(input);
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
            .ok_or(SocketError::NotConnected)?;
        target.enqueue_datagram(input, self.address())
    }

    fn enqueue_datagram(
        &self,
        input: &[u8],
        source: Option<UnixAddress>,
    ) -> Result<usize, SocketError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(input.len())
            .map_err(|_| SocketError::NoMemory)?;
        bytes.extend_from_slice(input);
        let mut state = self.state.lock();
        let SocketState::Datagram { messages, .. } = &mut *state else {
            return Err(SocketError::WrongType);
        };
        messages.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
        messages.push_back(Datagram { bytes, source });
        drop(state);
        self.notify();
        Ok(input.len())
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        let state = self.state.lock();
        match &*state {
            SocketState::Initial => SocketPollState {
                readable: false,
                writable: false,
                hangup: false,
                error: false,
            },
            SocketState::Listening { pending, .. } => SocketPollState {
                readable: !pending.is_empty(),
                writable: false,
                hangup: false,
                error: false,
            },
            SocketState::Stream {
                receive, transmit, ..
            } => {
                let read = receive
                    .as_ref()
                    .map(|end| end.pipe().poll_state(PipeDirection::Read));
                let write = transmit
                    .as_ref()
                    .map(|end| end.pipe().poll_state(PipeDirection::Write));
                SocketPollState {
                    readable: read.is_none_or(|state| state.readable),
                    writable: write.is_some_and(|state| state.writable),
                    hangup: read.is_some_and(|state| state.hangup),
                    error: write.is_some_and(|state| state.error),
                }
            }
            SocketState::Datagram { messages, .. } => SocketPollState {
                readable: !messages.is_empty(),
                writable: true,
                hangup: false,
                error: false,
            },
        }
    }

    /// @description 投影 socket 所有可能无条件返回或被请求的 poll 状态变化 generation。
    ///
    /// @param _events poll interest；stream 的 HUP/ERR 无条件返回，因此仍观察收发两侧。
    /// @return 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self, _events: i16) -> u64 {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => {
                // HUP/ERR 不受 requested mask 限制，因此两侧 generation 都必须参与；否则只关注
                // EPOLLIN 的 ET watcher 会在 peer write-close 时因 generation 未变化而漏掉 HUP。
                let read = receive.as_ref().map_or(0, |end| {
                    end.pipe().readiness_generation(PipeDirection::Read)
                });
                let write = transmit.as_ref().map_or(0, |end| {
                    end.pipe().readiness_generation(PipeDirection::Write)
                });
                read.max(write)
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
    pub(in crate::socket) fn wait_sources(&self) -> Vec<SocketWaitSource> {
        let state = self.state.lock();
        match &*state {
            SocketState::Stream {
                receive, transmit, ..
            } => vec![
                receive.as_ref().map(|end| SocketWaitSource::Data {
                    pipe: end.pipe(),
                    direction: PipeDirection::Read,
                }),
                transmit.as_ref().map(|end| SocketWaitSource::Data {
                    pipe: end.pipe(),
                    direction: PipeDirection::Write,
                }),
            ]
            .into_iter()
            .flatten()
            .collect(),
            _ => vec![SocketWaitSource::Notification(self.notify_read.pipe())],
        }
    }

    fn notify(&self) {
        self.notify_write.signal_readiness();
    }

    fn consume_notify(&self) {
        self.consume_wait_notifications();
    }

    /// @description 排空 listener/datagram 的内部 readiness edge；stream 的 wait source 是真实 data Pipe，禁止从此消费。
    ///
    /// @return 无返回值；实际 socket readiness 由随后的 level recheck 决定。
    pub(in crate::socket) fn consume_wait_notifications(&self) {
        if matches!(*self.state.lock(), SocketState::Stream { .. }) {
            return;
        }
        self.notify_read.drain_readiness();
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
        let mut namespace = NAMESPACE.call_once(|| Mutex::new(BTreeMap::new())).lock();
        if namespace
            .get(&address)
            .is_some_and(|entry| entry.strong_count() == 0)
        {
            namespace.remove(&address);
        }
    }
}
