use alloc::sync::Arc;

use crate::ipc::PipeEnd;

use super::{BoundAddress, SocketState, UnixSocket};
use crate::socket::{SocketError, SocketType, UnixCredentials};

impl UnixSocket {
    /// @description 原子建立 client/server stream endpoints 并发布到 listener backlog。
    /// @param client 发起连接且仍处于 Initial 的 endpoint。
    /// @param listener 已进入 Listening 的目标 endpoint。
    /// @param server 尚未发布的 accepted endpoint。
    /// @param client_to_server client 写、server 读的 Pipe endpoints。
    /// @param server_to_client server 写、client 读的 Pipe endpoints。
    /// @param client_credentials connect transaction 捕获的 caller credentials。
    /// @return 两端状态与 backlog publication 全部成功。
    /// @errors 类型、状态、backlog 或内存约束不满足时不发布半连接。
    pub(crate) fn connect_stream(
        client: &Arc<Self>,
        listener: &Arc<Self>,
        server: Arc<Self>,
        client_to_server: (Arc<PipeEnd>, Arc<PipeEnd>),
        server_to_client: (Arc<PipeEnd>, Arc<PipeEnd>),
        client_credentials: UnixCredentials,
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
        let (client_receive, server_transmit) = super::stream::channel(server_to_client, client)?;
        let (server_receive, client_transmit) = super::stream::channel(client_to_server, &server)?;

        // 1. 所有 fallible work 在 publication 前完成。
        // 2. 两端 transport 和 accepted address 在 listener lock 内一次提交。
        // 3. 最后发布 backlog 并在解锁后唤醒，避免观察到半连接或锁内 callback。
        *client_state = SocketState::Stream {
            receive: Some(client_receive),
            transmit: Some(client_transmit),
            peer: listener.address(),
            peer_credentials: Some(listener.credentials),
        };
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
        pending.push_back(server);
        drop(listener_state);
        drop(client_state);
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
            SocketType::Datagram => {
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
        let SocketState::Listening { pending, .. } = &mut *state else {
            return Err(SocketError::Invalid);
        };
        let accepted = pending.pop_front().ok_or(SocketError::Again)?;
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
