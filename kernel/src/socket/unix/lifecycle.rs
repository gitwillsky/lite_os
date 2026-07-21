use alloc::sync::Arc;

use crate::ipc::PipeEnd;

use super::{BoundAddress, SocketState, UnixSocket};
use crate::socket::{SocketError, SocketType, UnixCredentials};

use super::stream_backlog::{BacklogReservation, StagedConnection};

enum CapacityReservation {
    Reserved(BacklogReservation<Arc<UnixSocket>>),
    Staged(StagedConnection<Arc<UnixSocket>>),
}

struct ConnectGuard {
    // Some 是 client Connecting state 的唯一 owner。所有 error/OOM exit 由 Drop 还原 Initial；
    // commit 取走 capability。缺失它会让失败 transaction 永久留下不可重试 endpoint。
    client: Option<Arc<UnixSocket>>,
    listener: Arc<UnixSocket>,
    capacity: Option<CapacityReservation>,
}

impl ConnectGuard {
    fn begin(client: &Arc<UnixSocket>, listener: &Arc<UnixSocket>) -> Result<Self, SocketError> {
        let mut state = client.state.lock();
        if !matches!(*state, SocketState::Initial) {
            return Err(SocketError::AlreadyConnected);
        }
        *state = SocketState::Connecting;
        drop(state);
        let mut guard = Self {
            client: Some(client.clone()),
            listener: listener.clone(),
            capacity: None,
        };
        let reservation = {
            let mut listener_state = listener.state.lock();
            let SocketState::Listening { backlog } = &mut *listener_state else {
                return Err(SocketError::ConnectionRefused);
            };
            backlog.reserve().map_err(|_| SocketError::Again)?
        };
        guard.capacity = Some(CapacityReservation::Reserved(reservation));
        Ok(guard)
    }

    fn stage(&mut self, server: Arc<UnixSocket>) -> Result<(), SocketError> {
        let reservation = match self.capacity.take() {
            Some(CapacityReservation::Reserved(reservation)) => reservation,
            _ => panic!("AF_UNIX connect capacity staged twice"),
        };
        match reservation.try_stage(server) {
            Ok(staged) => {
                self.capacity = Some(CapacityReservation::Staged(staged));
                Ok(())
            }
            Err((_, reservation)) => {
                self.capacity = Some(CapacityReservation::Reserved(reservation));
                Err(SocketError::NoMemory)
            }
        }
    }

    fn commit(mut self, state: SocketState) {
        let client = self
            .client
            .as_ref()
            .expect("AF_UNIX connect guard consumed twice");
        let staged = match self.capacity.take() {
            Some(CapacityReservation::Staged(staged)) => staged,
            _ => panic!("AF_UNIX connect committed without staged backlog node"),
        };
        let mut client_state = client.state.lock();
        assert!(
            matches!(*client_state, SocketState::Connecting),
            "AF_UNIX connect transaction lost client ownership"
        );
        let mut listener_state = self.listener.state.lock();
        let SocketState::Listening { backlog } = &mut *listener_state else {
            panic!("AF_UNIX reserved listener changed state");
        };
        *client_state = state;
        backlog.commit(staged);
        drop(listener_state);
        drop(client_state);
        self.client.take();
    }
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        if let Some(capacity) = self.capacity.take() {
            let reservation = match capacity {
                CapacityReservation::Reserved(reservation) => reservation,
                CapacityReservation::Staged(staged) => staged.into_reservation(),
            };
            let mut listener_state = self.listener.state.lock();
            let SocketState::Listening { backlog } = &mut *listener_state else {
                panic!("AF_UNIX reserved listener changed state during rollback");
            };
            backlog.rollback(reservation);
        }
        let Some(client) = self.client.take() else {
            return;
        };
        let mut state = client.state.lock();
        assert!(
            matches!(*state, SocketState::Connecting),
            "AF_UNIX failed connect lost transaction state"
        );
        *state = SocketState::Initial;
    }
}

struct PreparedClientStream {
    receive: Arc<super::stream::StreamReceive>,
    transmit: Arc<super::stream::StreamTransmit>,
    peer: Option<super::UnixAddress>,
    peer_credentials: UnixCredentials,
}

impl UnixSocket {
    /// @description 原子建立 client/server stream endpoints 并发布到 listener backlog。
    /// @param client 发起连接且仍处于 Initial 的 endpoint。
    /// @param listener 已进入 Listening 的目标 endpoint。
    /// @param resources 只在 listener capacity reservation 成功后调用的 transport factory。
    /// @param client_credentials connect transaction 捕获的 caller credentials。
    /// @return 两端状态与 backlog publication 全部成功。
    /// @errors 类型、状态、backlog 或内存约束不满足时不发布半连接。
    pub(crate) fn connect_stream<F>(
        client: &Arc<Self>,
        listener: &Arc<Self>,
        client_credentials: UnixCredentials,
        resources: F,
    ) -> Result<(), SocketError>
    where
        F: FnOnce() -> Result<crate::socket::UnixConnectResources, SocketError>,
    {
        if client.socket_type != SocketType::Stream || listener.socket_type != SocketType::Stream {
            return Err(SocketError::WrongType);
        }
        let mut guard = ConnectGuard::begin(client, listener)?;
        let resources = resources()?;
        let server = UnixSocket::new(
            SocketType::Stream,
            resources.server_notify,
            listener.credentials,
            crate::id::next_runtime_object_id(),
        )?;
        let (client_receive, server_transmit) =
            super::stream::channel(resources.server_to_client, client)?;
        let (server_receive, client_transmit) =
            super::stream::channel(resources.client_to_server, &server)?;
        *server.state.lock() = SocketState::Stream {
            receive: Some(server_receive),
            transmit: Some(server_transmit),
            peer: client.address(),
            peer_credentials: Some(client_credentials),
        };
        *server.address.lock() = listener.address().map(|visible| BoundAddress {
            visible,
            binding: None,
        });
        let client_stream = PreparedClientStream {
            receive: client_receive,
            transmit: client_transmit,
            peer: listener.address(),
            peer_credentials: listener.credentials,
        };
        guard.stage(server)?;

        // 1. backlog reservation 在线性化点先于全部大块/可失败分配。
        // 2. server state 与 queue node 都在锁外完整准备。
        // 3. client lock 内只执行无分配 publication；pending 随后即可被 accept 安全观察。
        guard.commit(SocketState::Stream {
            receive: Some(client_stream.receive),
            transmit: Some(client_stream.transmit),
            peer: client_stream.peer,
            peer_credentials: Some(client_stream.peer_credentials),
        });
        // connect 会把 persistent epoll source 从 socket notification 重绑到两个
        // stream data Pipe。缺失该 edge 时，另一线程已阻塞的 epoll_wait
        // 会继续等待 Initial-state source，永久错过新 transport。
        client.notify();
        listener.notify();
        Ok(())
    }

    /// @description 将两个未发布 AF_UNIX endpoints 建立为 socketpair。
    /// @param first 第一端。
    /// @param second 第二端。
    /// @param first_to_second 第一端到第二端的 Pipe。
    /// @param second_to_first 第二端到第一端的 Pipe。
    /// @return 对称 peer state 提交成功。
    /// @errors 当前构造路径已预分配全部资源，不产生运行时错误。
    pub(crate) fn pair(
        first: &Arc<Self>,
        second: &Arc<Self>,
        first_to_second: (Arc<PipeEnd>, Arc<PipeEnd>),
        second_to_first: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<(), SocketError> {
        match first.socket_type {
            SocketType::Stream => {
                let (second_receive, first_transmit) =
                    super::stream::channel(first_to_second, second)?;
                let (first_receive, second_transmit) =
                    super::stream::channel(second_to_first, first)?;
                *first.state.lock() = SocketState::Stream {
                    receive: Some(first_receive),
                    transmit: Some(first_transmit),
                    peer: None,
                    peer_credentials: Some(second.credentials),
                };
                *second.state.lock() = SocketState::Stream {
                    receive: Some(second_receive),
                    transmit: Some(second_transmit),
                    peer: None,
                    peer_credentials: Some(first.credentials),
                };
            }
            SocketType::Datagram | SocketType::SeqPacket => {
                if let SocketState::Datagram {
                    peer,
                    peer_credentials,
                    ..
                } = &mut *first.state.lock()
                {
                    *peer = Some(Arc::downgrade(second));
                    *peer_credentials = Some(second.credentials);
                }
                if let SocketState::Datagram {
                    peer,
                    peer_credentials,
                    ..
                } = &mut *second.state.lock()
                {
                    *peer = Some(Arc::downgrade(first));
                    *peer_credentials = Some(first.credentials);
                }
            }
            SocketType::Raw => unreachable!("AF_UNIX raw pair crossed Socket facade"),
        }
        Ok(())
    }

    /// @description 连接 datagram endpoint 并冻结 peer credentials。
    /// @param peer_socket 目标 datagram endpoint。
    /// @return peer identity publication 成功。
    /// @errors 任一 endpoint 不是 datagram 时返回 WrongType。
    pub(crate) fn connect_datagram(&self, peer_socket: &Arc<Self>) -> Result<(), SocketError> {
        if self.socket_type != SocketType::Datagram
            || peer_socket.socket_type != SocketType::Datagram
        {
            return Err(SocketError::WrongType);
        }
        let mut state = self.state.lock();
        let SocketState::Datagram {
            peer,
            peer_credentials,
            ..
        } = &mut *state
        else {
            return Err(SocketError::WrongType);
        };
        *peer = Some(Arc::downgrade(peer_socket));
        *peer_credentials = Some(peer_socket.credentials);
        drop(state);
        self.notify();
        Ok(())
    }

    /// @description 从 listener backlog 原子取出一个 accepted endpoint。
    /// @return backlog 头部 endpoint。
    /// @errors 非 listener 或 backlog 为空时返回明确错误。
    pub(crate) fn accept(&self) -> Result<Arc<Self>, SocketError> {
        let mut state = self.state.lock();
        let SocketState::Listening { backlog } = &mut *state else {
            return Err(SocketError::Invalid);
        };
        let accepted = backlog.pop().ok_or(SocketError::Again)?;
        drop(state);
        self.consume_notify();
        Ok(accepted)
    }

    /// @description 投影连接建立时冻结的 peer credentials。
    /// @return stream/datagram peer 的 Linux `ucred`。
    /// @errors 尚未连接或 listener endpoint 返回 NotConnected。
    pub(crate) fn peer_credentials(&self) -> Result<UnixCredentials, SocketError> {
        match &*self.state.lock() {
            SocketState::Stream {
                peer_credentials, ..
            }
            | SocketState::Datagram {
                peer_credentials, ..
            } => peer_credentials.ok_or(SocketError::NotConnected),
            _ => Err(SocketError::NotConnected),
        }
    }
}
