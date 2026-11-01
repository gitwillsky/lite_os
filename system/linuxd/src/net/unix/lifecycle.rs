use alloc::sync::Arc;

use super::{BoundAddress, DatagramQueue, PacketShutdown, SocketState, UnixSocket};
use crate::net::{SocketError, SocketType, UnixConnectionData, UnixCredentials};

use super::connection_backlog::{BacklogReservation, StagedConnection};

enum CapacityReservation {
    Reserved(BacklogReservation<Arc<UnixSocket>>),
    Staged(StagedConnection<Arc<UnixSocket>>),
}

/// 未发布构造只拥有 listener 的 capacity，不修改 client 可观察状态。
/// 同一 client 的并发 connect 在最后双端 publication 重查真正胜出的连接。
struct ConnectGuard {
    client: Arc<UnixSocket>,
    listener: Arc<UnixSocket>,
    // 实际 OFD 是构造期间的 root；cached endpoint Arc 无法阻止最后 OFD close。
    _listener_file: Arc<dyn super::UnixPassedFile>,
    capacity: Option<CapacityReservation>,
}

fn connectable_shutdown(
    state: &SocketState,
    kind: SocketType,
) -> Result<PacketShutdown, SocketError> {
    match state {
        SocketState::Initial { shutdown } => Ok(*shutdown),
        SocketState::Datagram {
            messages,
            peer,
            shutdown,
            ..
        } if kind == SocketType::SeqPacket && peer.is_none() && messages.is_empty() => {
            Ok(*shutdown)
        }
        SocketState::Stream { .. } | SocketState::Datagram { peer: Some(_), .. } => {
            Err(SocketError::AlreadyConnected)
        }
        _ => Err(SocketError::Invalid),
    }
}
impl ConnectGuard {
    fn begin(client: &Arc<UnixSocket>, listener: &Arc<UnixSocket>) -> Result<Self, SocketError> {
        let listener_file = listener.pin_description()?;
        let mut guard = Self {
            client: client.clone(),
            listener: listener.clone(),
            _listener_file: listener_file,
            capacity: None,
        };
        let reservation = {
            let mut listener_state = listener.state.lock();
            let SocketState::Listening {
                backlog, shutdown, ..
            } = &mut *listener_state
            else {
                return Err(SocketError::ConnectionRefused);
            };
            if shutdown.read {
                return Err(SocketError::ConnectionRefused);
            }
            backlog.reserve().map_err(|_| SocketError::Again)?
        };
        guard.capacity = Some(CapacityReservation::Reserved(reservation));
        // Native listener/backlog 错误先于 client 状态；准备阶段只借用，不发布临时 state。
        connectable_shutdown(&client.state.lock(), client.socket_type)?;
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
    fn commit(mut self, mut prepared: SocketState) -> Result<(), SocketError> {
        let mut client_state = self.client.state.lock();
        let mut listener_state = self.listener.state.lock();
        let SocketState::Listening {
            backlog,
            credentials,
            shutdown,
            ..
        } = &mut *listener_state
        else {
            panic!("AF_UNIX pinned listener changed state");
        };
        if shutdown.read {
            return Err(SocketError::ConnectionRefused);
        }
        let previous_shutdown = connectable_shutdown(&client_state, self.client.socket_type)?;
        let peer_credentials = match &mut prepared {
            SocketState::Stream {
                receive,
                transmit,
                peer_credentials,
                ..
            } => {
                // Preparation 期间可能发生合法 shutdown；只在实际 publication 使用当前事实。
                if previous_shutdown.read {
                    receive.as_ref().unwrap().shutdown_read();
                }
                if previous_shutdown.write {
                    transmit.as_ref().unwrap().stop_writes();
                }
                peer_credentials
            }
            SocketState::Datagram {
                shutdown,
                peer_credentials,
                ..
            } => {
                *shutdown = previous_shutdown;
                peer_credentials
            }
            _ => unreachable!(),
        };
        *peer_credentials = Some(credentials.clone());
        let staged = match self.capacity.take() {
            Some(CapacityReservation::Staged(staged)) => staged,
            _ => panic!("AF_UNIX connect committed without staged backlog node"),
        };
        staged
            .value()
            .set_pass_credentials(self.listener.pass_credentials());
        let retired = core::mem::replace(&mut *client_state, prepared);
        backlog.commit(staged);
        drop(listener_state);
        drop(client_state);
        drop(retired);
        Ok(())
    }
}
impl Drop for ConnectGuard {
    fn drop(&mut self) {
        let Some(capacity) = self.capacity.take() else {
            return;
        };
        // staged node/server 在取得 listener guard 前退休，避免 control/peer cleanup 锁反转。
        let reservation = match capacity {
            CapacityReservation::Reserved(reservation) => reservation,
            CapacityReservation::Staged(staged) => staged.into_reservation(),
        };
        let mut state = self.listener.state.lock();
        let SocketState::Listening { backlog, .. } = &mut *state else {
            panic!("AF_UNIX pinned listener changed state during rollback");
        };
        backlog.rollback(reservation);
        drop(state);
        self.listener.notify();
    }
}

impl UnixSocket {
    /// 原子建立 client/server stream endpoints 并发布到 listener backlog。
    ///
    /// # Parameters
    ///
    /// - `client`: 发起连接且仍处于 Initial 的 endpoint。
    /// - `listener`: 已进入 Listening 的目标 endpoint。
    /// - `resources`: 只在 listener capacity reservation 成功后调用的 transport factory。
    /// - `client_credentials`: connect transaction 捕获的 caller credentials。
    ///
    /// # Returns
    ///
    /// 两端状态与 backlog publication 全部成功。
    ///
    /// # Errors
    ///
    /// 类型、状态、backlog 或内存约束不满足时不发布半连接。
    pub(crate) fn connect_oriented<F>(
        client: &Arc<Self>,
        listener: &Arc<Self>,
        client_credentials: UnixCredentials,
        resources: F,
    ) -> Result<(), SocketError>
    where
        F: FnOnce() -> Result<crate::net::UnixConnectResources, SocketError>,
    {
        if client.socket_type != listener.socket_type
            || !matches!(
                client.socket_type,
                SocketType::Stream | SocketType::SeqPacket
            )
        {
            return Err(SocketError::WrongType);
        }
        let mut guard = ConnectGuard::begin(client, listener)?;
        let resources = resources()?;
        let server = UnixSocket::new(
            client.socket_type,
            resources.server_notify,
            crate::id::next_runtime_object_id(),
            listener.net_namespace.clone(),
        )?;
        let client_state = match resources.data {
            UnixConnectionData::Stream {
                first_to_second,
                second_to_first,
            } => {
                assert_eq!(
                    client.socket_type,
                    SocketType::Stream,
                    "typed stream resource mismatch"
                );
                let (client_receive, server_transmit) =
                    super::stream::channel(second_to_first, client, server.node_id())?;
                let (server_receive, client_transmit) =
                    super::stream::channel(first_to_second, &server, client.node_id())?;
                *server.state.lock() = SocketState::Stream {
                    receive: Some(server_receive),
                    transmit: Some(server_transmit),
                    pending_error: None,
                    peer: client.clone(),
                    peer_credentials: Some(client_credentials),
                };
                SocketState::Stream {
                    receive: Some(client_receive),
                    transmit: Some(client_transmit),
                    pending_error: None,
                    peer: server.clone(),
                    peer_credentials: None,
                }
            }
            UnixConnectionData::Packet => {
                assert_eq!(
                    client.socket_type,
                    SocketType::SeqPacket,
                    "typed oriented packet resource mismatch"
                );
                *server.state.lock() = SocketState::Datagram {
                    messages: DatagramQueue::new(),
                    peer: Some(client.clone()),
                    peer_credentials: Some(client_credentials),
                    shutdown: PacketShutdown::default(),
                    pending_error: None,
                };
                SocketState::Datagram {
                    messages: DatagramQueue::new(),
                    peer: Some(server.clone()),
                    peer_credentials: None,
                    shutdown: PacketShutdown::default(),
                    pending_error: None,
                }
            }
        };
        *server.address.lock() = listener.address.lock().as_ref().map(|bound| BoundAddress {
            visible: bound.visible,
            binding: None,
            path_owner: bound.path_owner.clone(),
        });
        let ownership = super::rights_graph::PreparedBacklog::new(&server, listener)?.publish()?;
        // server 尚只有构造事务引用；实际归属先于 client transport 与 backlog 同时可观察。
        server.bind_backlog_owner(ownership);
        guard.stage(server)?;

        // 1. backlog reservation 在线性化点先于全部大块/可失败分配。
        // 2. server state 与 queue node 都在锁外完整准备。
        // 3. client lock 内只执行无分配 publication；pending 随后即可被 accept 安全观察。
        guard.commit(client_state)?;
        // connect 会把 persistent epoll source 从 socket notification 重绑到两个
        // stream data Pipe。缺失该 edge 时，另一线程已阻塞的 epoll_wait
        // 会继续等待 Initial-state source，永久错过新 transport。
        client.notify();
        listener.notify();
        Ok(())
    }

    /// 将两个未发布 AF_UNIX endpoints 建立为 socketpair。
    ///
    /// # Parameters
    ///
    /// - `first`: 第一端。
    /// - `second`: 第二端。
    /// - `data`: 上层为实际 socket type 准备的 typed stream/packet resources。
    ///
    /// # Returns
    ///
    /// 对称 peer state 提交成功。
    ///
    /// # Errors
    ///
    /// 当前构造路径已预分配全部资源，不产生运行时错误。
    pub(crate) fn pair(
        first: &Arc<Self>,
        second: &Arc<Self>,
        data: UnixConnectionData,
        credentials: UnixCredentials,
    ) -> Result<(), SocketError> {
        match first.socket_type {
            SocketType::Stream => {
                let UnixConnectionData::Stream {
                    first_to_second,
                    second_to_first,
                } = data
                else {
                    unreachable!("typed socketpair stream resources")
                };
                let (second_receive, first_transmit) =
                    super::stream::channel(first_to_second, second, first.node_id())?;
                let (first_receive, second_transmit) =
                    super::stream::channel(second_to_first, first, second.node_id())?;
                let mut first_state = first.state.lock();
                assert!(matches!(*first_state, SocketState::Initial { .. }));
                *first_state = SocketState::Stream {
                    receive: Some(first_receive),
                    transmit: Some(first_transmit),
                    pending_error: None,
                    peer: second.clone(),
                    peer_credentials: Some(credentials.clone()),
                };
                drop(first_state);
                let mut second_state = second.state.lock();
                assert!(matches!(*second_state, SocketState::Initial { .. }));
                *second_state = SocketState::Stream {
                    receive: Some(second_receive),
                    transmit: Some(second_transmit),
                    pending_error: None,
                    peer: first.clone(),
                    peer_credentials: Some(credentials.clone()),
                };
            }
            SocketType::Datagram | SocketType::SeqPacket => {
                assert!(matches!(data, UnixConnectionData::Packet));
                if let SocketState::Datagram {
                    peer,
                    peer_credentials,
                    ..
                } = &mut *first.state.lock()
                {
                    assert!(peer.is_none() && peer_credentials.is_none());
                    *peer = Some(second.clone());
                    *peer_credentials = Some(credentials.clone());
                }
                if let SocketState::Datagram {
                    peer,
                    peer_credentials,
                    ..
                } = &mut *second.state.lock()
                {
                    assert!(peer.is_none() && peer_credentials.is_none());
                    *peer = Some(first.clone());
                    *peer_credentials = Some(credentials.clone());
                }
            }
            SocketType::Raw => unreachable!("AF_UNIX raw pair crossed Socket facade"),
        }
        Ok(())
    }

    /// 从 listener backlog 原子取出一个 accepted endpoint。
    ///
    /// # Returns
    ///
    /// backlog 头部 endpoint。
    ///
    /// # Errors
    ///
    /// 非 listener 或 backlog 为空时返回明确错误。
    pub(crate) fn accept(&self) -> Result<Arc<Self>, SocketError> {
        let mut state = self.state.lock();
        let SocketState::Listening {
            backlog, shutdown, ..
        } = &mut *state
        else {
            return Err(SocketError::Invalid);
        };
        let accepted = backlog.pop().ok_or(if shutdown.read {
            SocketError::Invalid
        } else {
            SocketError::Again
        })?;
        drop(state);
        let accepted = accepted.into_value();
        self.consume_notify();
        // accept 释放实际 backlog capacity，唤醒等待该 listener 的 connect，锁外发布 edge。
        self.notify();
        Ok(accepted)
    }

    /// 投影连接建立时冻结的 peer credentials。
    ///
    /// # Returns
    ///
    /// connect/socketpair 捕获的 peer，或 listen 捕获的 caller；尚未安装时返回 None。
    pub(crate) fn peer_credentials(&self) -> Option<UnixCredentials> {
        match &*self.state.lock() {
            SocketState::Stream {
                peer_credentials, ..
            }
            | SocketState::Datagram {
                peer_credentials, ..
            } => peer_credentials.as_ref().cloned(),
            SocketState::Listening { credentials, .. } => Some(credentials.clone()),
            _ => None,
        }
    }
}
