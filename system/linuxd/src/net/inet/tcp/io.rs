use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketSet,
    socket::tcp::{self, State},
};

use super::{TcpConnection, TcpEndpointState, TcpMode, endpoint_id};
use crate::ipc::ReceiveBuffer;
use crate::net::inet::{InetSocket, NetworkStack};
use crate::net::{InetAddress, SocketError, SocketPollState};

/// 向 TCP send buffer 排队 partial stream bytes，并有界推进 egress。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `input`: kernel-owned input bytes。
///
/// # Returns
///
/// 本次实际排队字节数。
///
/// # Errors
///
/// 未连接、peer 关闭、pending error 或 buffer 满时返回标准 socket error。
pub(in crate::net::inet) fn send(socket: &InetSocket, input: &[u8]) -> Result<usize, SocketError> {
    let id = endpoint_id(socket);
    let stack = socket.stack()?;
    let written = stack.with_payload_loan(
        |network| {
            let state = network
                .tcp_endpoints
                .get_mut(&id)
                .ok_or(SocketError::NotConnected)?;
            if let Some(error) = state.pending_error.take() {
                return Err(error);
            }
            if !matches!(state.mode, TcpMode::Connected(_)) {
                return Err(match state.mode {
                    TcpMode::Connecting { terminal: true, .. } => SocketError::BrokenPipe,
                    TcpMode::Connecting { .. } => SocketError::Again,
                    _ => SocketError::BrokenPipe,
                });
            }
            if matches!(state.mode, TcpMode::Connected(connection) if connection.shutdown_write) {
                return Err(SocketError::BrokenPipe);
            }
            let handle = state.handles[0].handle;
            let tcp = core::mem::replace(
                network.sockets.get_mut::<tcp::Socket<'static>>(handle),
                super::placeholder_socket(),
            );
            Ok((handle, tcp))
        },
        |(_, tcp)| {
            if !tcp.may_send() {
                return Err(SocketError::BrokenPipe);
            }
            tcp.send_slice(input)
                .map_err(|_| SocketError::BrokenPipe)
                .and_then(|count| {
                    if count == 0 && !input.is_empty() {
                        Err(SocketError::Again)
                    } else {
                        Ok(count)
                    }
                })
        },
        |network, (handle, tcp)| {
            let placeholder =
                core::mem::replace(network.sockets.get_mut::<tcp::Socket<'static>>(handle), tcp);
            debug_assert_eq!(placeholder.state(), State::Closed);
        },
    )?;
    crate::devices::model::network::request_poll();
    Ok(written)
}

/// 接收或窥视 TCP stream bytes，并在 peer FIN 后投影 EOF。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `output`: kernel-owned output buffer。
/// - `peek`: 为 true 时不推进 receive sequence。
///
/// # Returns
///
/// copied length、同值 stream full length、peer 与无 ancillary local address。
///
/// # Errors
///
/// 未连接、暂无数据或 reset 时返回标准 socket error。
pub(in crate::net::inet) fn receive(
    socket: &InetSocket,
    output: &mut ReceiveBuffer<'_>,
    peek: bool,
) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
    let id = endpoint_id(socket);
    let stack = socket.stack()?;
    let receive_limit = output.remaining();
    if receive_limit == 0 {
        let network = stack.lock()?;
        let state = network
            .tcp_endpoints
            .get(&id)
            .ok_or(SocketError::NotConnected)?;
        if matches!(state.mode, TcpMode::Listening { .. }) {
            return Err(SocketError::NotConnected);
        }
        return Ok((
            0,
            0,
            InetAddress {
                address: Ipv4Addr::UNSPECIFIED,
                port: 0,
            },
            None,
        ));
    }
    // 真实 protocol buffer 的临时 loan；EOF 分支不取出 handle、不另保存生命周期。
    let (result, still_readable) = stack.with_payload_loan(
        |network| {
            let state = network
                .tcp_endpoints
                .get_mut(&id)
                .ok_or(SocketError::NotConnected)?;
            let connection = match state.mode {
                TcpMode::Connected(connection) => connection,
                TcpMode::Connecting { terminal: true, .. } => {
                    if let Some(error) = state.pending_error.take() {
                        return Err(error);
                    }
                    return Ok(None);
                }
                TcpMode::Connecting {
                    shutdown_read: true,
                    ..
                } => return Ok(None),
                TcpMode::Connecting { .. } => return Err(SocketError::Again),
                TcpMode::Closed { shutdown_read, .. } => {
                    if let Some(error) = state.pending_error.take() {
                        return Err(error);
                    }
                    return if shutdown_read {
                        Ok(None)
                    } else {
                        Err(SocketError::NotConnected)
                    };
                }
                _ => {
                    return Err(state
                        .pending_error
                        .take()
                        .unwrap_or(SocketError::NotConnected));
                }
            };
            let handle = state.handles[0].handle;
            let tcp = network.sockets.get::<tcp::Socket<'static>>(handle);
            // 空 recv 不等待、消耗 error 或触碰 RX。实际 queued bytes 优先于 SHUT_RD/RESET。
            let pending_error = if !tcp.can_recv() && !connection.peer_closed {
                state.pending_error.take()
            } else {
                None
            };
            let tcp = core::mem::replace(
                network.sockets.get_mut::<tcp::Socket<'static>>(handle),
                super::placeholder_socket(),
            );
            Ok(Some((handle, tcp, connection, pending_error)))
        },
        |loan| {
            let Some((_, tcp, connection, pending_error)) = loan else {
                return Ok((
                    (
                        0,
                        0,
                        InetAddress {
                            address: Ipv4Addr::UNSPECIFIED,
                            port: 0,
                        },
                        None,
                    ),
                    true,
                ));
            };
            let count = if tcp.can_recv() {
                if peek {
                    let bytes = tcp
                        .peek(output.remaining())
                        .map_err(|_| SocketError::ConnectionReset)?;
                    output.append(bytes)
                } else {
                    tcp.recv(|bytes| {
                        let count = output.append(bytes);
                        (count, count)
                    })
                    .map_err(|_| SocketError::ConnectionReset)?
                }
            } else if connection.peer_closed {
                0
            } else if let Some(error) = *pending_error {
                return Err(error);
            } else if connection.shutdown_read || !tcp.may_recv() {
                0
            } else {
                return Err(SocketError::Again);
            };
            Ok((
                (count, count, connection.peer, None),
                tcp.can_recv() || connection.shutdown_read || !tcp.may_recv(),
            ))
        },
        |network, loan| {
            let Some((handle, tcp, _, _)) = loan else {
                return;
            };
            let placeholder =
                core::mem::replace(network.sockets.get_mut::<tcp::Socket<'static>>(handle), tcp);
            debug_assert_eq!(placeholder.state(), State::Closed);
        },
    )?;
    if !peek && !still_readable {
        socket.consume_notify();
    }
    Ok(result)
}

/// 从唯一 TCP endpoint state 投影 OFD readiness。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// listener/connect/connected 对应的 poll state。
///
/// # Errors
///
/// endpoint 不可用时返回 error readiness。
pub(in crate::net::inet) fn poll_state(socket: &InetSocket) -> SocketPollState {
    let Ok(stack) = socket.stack() else {
        return SocketPollState::error();
    };
    let Ok(network) = stack.lock() else {
        return SocketPollState::error();
    };
    network
        .tcp_endpoints
        .get(&endpoint_id(socket))
        .map_or_else(SocketPollState::error, |state| {
            state.poll_state(&network.sockets)
        })
}

/// 在 deferred source 通知中无等待地投影 TCP readiness。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// owner 可立即观察且 SocketSet 完整时返回状态，否则返回 `None`。
///
/// # Errors
///
/// 只投影实际endpoint状态；设备诊断不生成socket协议错误。
pub(in crate::net::inet) fn try_poll_state(socket: &InetSocket) -> Option<SocketPollState> {
    let network = socket.try_observe_stack()?;
    let readiness = network
        .tcp_endpoints
        .get(&endpoint_id(socket))
        .map_or_else(SocketPollState::error, |state| {
            state.poll_state(&network.sockets)
        });
    Some(readiness)
}

impl TcpEndpointState {
    /// 在已持有 NetworkStack lock 时计算 endpoint readiness。
    ///
    /// # Parameters
    ///
    /// - `sockets`: 已锁定协议栈的 native SocketSet，只读借用不包含 endpoint 树。
    ///
    /// # Returns
    ///
    /// 不注册 waiter 的状态快照。
    pub(in crate::net::inet) fn poll_state(&self, sockets: &SocketSet<'static>) -> SocketPollState {
        match self.mode {
            TcpMode::Closed { shutdown_read, .. } => SocketPollState {
                readable: shutdown_read,
                writable: true,
                hangup: true,
                read_closed: shutdown_read,
                error: self.pending_error.is_some(),
            },
            TcpMode::Listening { .. } => SocketPollState {
                readable: self.handles.iter().any(|entry| entry.passive.is_some()),
                writable: false,
                hangup: false,
                read_closed: false,
                error: false,
            },
            TcpMode::Connecting {
                shutdown_read,
                shutdown_write,
                ..
            } => {
                let state = sockets
                    .get::<tcp::Socket<'static>>(self.handles[0].handle)
                    .state();
                SocketPollState {
                    readable: shutdown_read || state == State::Closed,
                    writable: matches!(state, State::Established | State::Closed),
                    hangup: (shutdown_read && shutdown_write) || state == State::Closed,
                    read_closed: shutdown_read || state == State::Closed,
                    error: self.pending_error.is_some(),
                }
            }
            TcpMode::Connected(connection) => {
                let socket = sockets.get::<tcp::Socket<'static>>(self.handles[0].handle);
                let closed = socket.state() == State::Closed;
                SocketPollState {
                    readable: connection.shutdown_read || socket.can_recv() || !socket.may_recv(),
                    writable: connection.shutdown_write || socket.can_send() || !socket.may_send(),
                    hangup: closed
                        || ((connection.shutdown_read || connection.peer_closed)
                            && (connection.shutdown_write || !socket.may_send())),
                    read_closed: connection.shutdown_read
                        || matches!(
                            socket.state(),
                            State::CloseWait
                                | State::LastAck
                                | State::Closing
                                | State::TimeWait
                                | State::Closed
                        ),
                    error: closed && self.pending_error.is_some(),
                }
            }
        }
    }
}

/// 在协议 poll 内提交 connect/FIN/reset 状态。
///
/// # Parameters
///
/// - `network`: 唯一协议栈 owner。
///
/// # Errors
///
/// 状态不变量破坏时 fail-stop。
/// 实际SocketSet的纯时间phase，先于任何FIB/NIC filter；无新锁/分配/Context。
pub(in crate::net::inet) fn maintain_clocks(
    network: &mut NetworkStack,
    timestamp: smoltcp::time::Instant,
) {
    for (_, socket) in network.sockets.iter_mut() {
        if let smoltcp::socket::Socket::Tcp(socket) = socket {
            socket.maintain(timestamp);
        }
    }
    maintain(network, None);
}

pub(in crate::net::inet) fn maintain(
    network: &mut NetworkStack,
    reset: Option<super::super::device::TcpReset>,
) {
    let NetworkStack {
        tcp_endpoints,
        sockets,
        ..
    } = network;
    tcp_endpoints.for_each_mut(|_, state| {
        if state.orphaned {
            return;
        }
        match &mut state.mode {
            TcpMode::Connecting {
                bound,
                terminal,
                shutdown_read,
                shutdown_write,
                ..
            } => {
                let tcp = sockets.get_mut::<tcp::Socket<'static>>(state.handles[0].handle);
                match tcp.state() {
                    State::Established | State::CloseWait => {
                        let mut connection = TcpConnection::established(tcp, *bound);
                        connection.shutdown_read = *shutdown_read;
                        connection.shutdown_write = *shutdown_write;
                        // ACK 完成后才提交 deferred FIN；之前不可发布 connected 或 POLLOUT。
                        if let Some(error) = connection.observe(tcp, reset) {
                            state.pending_error = Some(error);
                        }
                        state.mode = TcpMode::Connected(connection);
                    }
                    State::Closed if !*terminal => {
                        *terminal = true;
                        state.pending_error = Some(
                            if tcp.failure_reason() == Some(tcp::FailureReason::TimedOut) {
                                SocketError::TimedOut
                            } else {
                                SocketError::ConnectionRefused
                            },
                        );
                    }
                    _ => {}
                }
            }
            TcpMode::Connected(connection) => {
                let tcp = sockets.get_mut::<tcp::Socket<'static>>(state.handles[0].handle);
                if let Some(error) = connection.observe(tcp, reset) {
                    state.pending_error = Some(error);
                }
            }
            TcpMode::Listening { endpoint, .. } => {
                for entry in &mut state.handles {
                    let tcp = sockets.get_mut::<tcp::Socket<'static>>(entry.handle);
                    if let Some(policy) = &mut entry.passive {
                        // 已建立 child 留在实际 accept queue；RST 不得把它改回监听槽。
                        if let Some(error) = policy.connection.observe(tcp, reset) {
                            policy.pending_error = Some(error);
                        }
                        continue;
                    }
                    match tcp.state() {
                        State::Established | State::CloseWait if entry.passive.is_none() => {
                            entry.passive = Some(super::PassivePolicy {
                                timeouts: state.timeouts.snapshot(),
                                options: state.options,
                                connection: TcpConnection::established(tcp, *endpoint),
                                pending_error: None,
                            });
                            state.options.apply_tcp(tcp);
                        }
                        State::Closed => {
                            state.options.apply_tcp(tcp);
                            tcp.listen(*endpoint)
                                .expect("existing passive endpoint must remain valid");
                        }
                        _ => {}
                    }
                }
            }
            TcpMode::Closed { .. } => {}
        }
    });
}

/// 报文owner已消费reset或协议自然结束后回收Closed orphan，不等待网卡交付。
pub(in crate::net::inet) fn reap_orphans(network: &mut NetworkStack) {
    let NetworkStack {
        tcp_endpoints,
        sockets,
        tcp_ports,
        ..
    } = network;
    tcp_endpoints.retain(|_, state| {
        if !state.orphaned
            || state.handles.iter().any(|handle| {
                let tcp = sockets.get::<tcp::Socket<'static>>(handle.handle);
                tcp.state() != State::Closed || tcp.remote_endpoint().is_some()
            })
        {
            return true;
        }
        for entry in &state.handles {
            let handle = entry.handle;
            sockets.remove(handle);
        }
        if let Some(lease) = state.port_lease {
            tcp_ports.release(lease);
        }
        false
    });
}

/// 原子读取并清除 TCP pending error，供 `SO_ERROR` 使用。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
///
/// # Returns
///
/// 尚未消费的错误；没有时为 None。
///
/// # Errors
///
/// stack/endpoint 不可用时按无 pending error 处理。
pub(in crate::net::inet) fn take_error(socket: &InetSocket) -> Option<SocketError> {
    socket
        .stack()
        .ok()?
        .lock()
        .ok()?
        .tcp_endpoints
        .get_mut(&endpoint_id(socket))?
        .pending_error
        .take()
}

/// 按 Linux `SHUT_RD/WR/RDWR` 更新 TCP half-close 状态。
///
/// # Parameters
///
/// - `socket`: TCP facade identity。
/// - `how`: 0、1 或 2；syscall 层已完成范围校验。
///
/// # Returns
///
/// 成功提交 half-close 返回 unit。
///
/// # Errors
///
/// 协议已 closed 时仍更新 read shutdown，再返回 `NotConnected`。
pub(in crate::net::inet) fn shutdown(socket: &InetSocket, how: usize) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = socket.stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get(&id)
        .ok_or(SocketError::NotConnected)?;
    let mode = state.mode;
    let phase = state
        .handles
        .first()
        .map(|entry| {
            network
                .sockets
                .get::<tcp::Socket<'static>>(entry.handle)
                .state()
        })
        .unwrap_or(State::Closed);
    let result = match mode {
        TcpMode::Listening { .. } => {
            if matches!(how, 0 | 2) {
                super::lifecycle::disconnect_locked(&mut network, id);
            }
            Ok(())
        }
        TcpMode::Connecting {
            terminal: false, ..
        } if phase == State::SynSent => {
            super::lifecycle::disconnect_locked(&mut network, id);
            Ok(())
        }
        _ => {
            let closed = phase == State::Closed;
            let state = network
                .tcp_endpoints
                .get_mut(&id)
                .expect("shutdown lost TCP endpoint");
            if matches!(how, 0 | 2) {
                match &mut state.mode {
                    TcpMode::Connected(connection) => connection.shutdown_read = true,
                    TcpMode::Closed { shutdown_read, .. }
                    | TcpMode::Connecting { shutdown_read, .. } => *shutdown_read = true,
                    _ => {}
                }
            }
            if matches!(how, 1 | 2) {
                match &mut state.mode {
                    TcpMode::Connected(connection) => connection.shutdown_write = true,
                    TcpMode::Connecting { shutdown_write, .. } => *shutdown_write = true,
                    _ => {}
                }
                if matches!(phase, State::Established | State::CloseWait) {
                    let handle = state.handles[0].handle;
                    network
                        .sockets
                        .get_mut::<tcp::Socket<'static>>(handle)
                        .close();
                }
            }
            if closed {
                Err(SocketError::NotConnected)
            } else {
                Ok(())
            }
        }
    };
    drop(network);
    crate::devices::model::network::request_poll();
    result
}
