use core::net::Ipv4Addr;

use smoltcp::socket::tcp::{self, State};

use super::{TcpEndpointState, TcpMode, endpoint_id};
use crate::ipc::ReceiveBuffer;
use crate::socket::inet::{InetSocket, NetworkStack, from_ip, stack};
use crate::socket::{InetAddress, SocketError, SocketPollState};

/// @description 向 TCP send buffer 排队 partial stream bytes，并有界推进 egress。
/// @param socket TCP facade identity。
/// @param input kernel-owned input bytes。
/// @return 本次实际排队字节数。
/// @errors 未连接、peer 关闭、pending error 或 buffer 满时返回标准 socket error。
pub(in crate::socket::inet) fn send(
    socket: &InetSocket,
    input: &[u8],
) -> Result<usize, SocketError> {
    let id = endpoint_id(socket);
    let stack = stack()?;
    let written = stack.with_payload_loan(
        |network| {
            let state = network
                .tcp_endpoints
                .get(&id)
                .ok_or(SocketError::NotConnected)?;
            if let Some(error) = state.pending_error {
                return Err(error);
            }
            if !matches!(state.mode, TcpMode::Connected { .. }) {
                return Err(match state.mode {
                    TcpMode::Connecting => SocketError::Again,
                    _ => SocketError::NotConnected,
                });
            }
            let handle = state.handles[0];
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
    crate::drivers::network::request_poll();
    Ok(written)
}

/// @description 接收或窥视 TCP stream bytes，并在 peer FIN 后投影 EOF。
/// @param socket TCP facade identity。
/// @param output kernel-owned output buffer。
/// @param peek 为 true 时不推进 receive sequence。
/// @return copied length、同值 stream full length、peer 与无 ancillary local address。
/// @errors 未连接、暂无数据或 reset 时返回标准 socket error。
pub(in crate::socket::inet) fn receive(
    socket: &InetSocket,
    output: &mut ReceiveBuffer<'_>,
    peek: bool,
) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
    let id = endpoint_id(socket);
    let stack = stack()?;
    let (result, still_readable) = stack.with_payload_loan(
        |network| {
            let state = network
                .tcp_endpoints
                .get(&id)
                .ok_or(SocketError::NotConnected)?;
            let (peer_closed, shutdown_read) = match state.mode {
                TcpMode::Connected {
                    peer_closed,
                    shutdown_read,
                } => (peer_closed, shutdown_read),
                TcpMode::Connecting => return Err(SocketError::Again),
                _ => return Err(SocketError::NotConnected),
            };
            let pending_error = state.pending_error;
            if shutdown_read {
                return Ok((None, pending_error, peer_closed));
            }
            let handle = state.handles[0];
            let tcp = core::mem::replace(
                network.sockets.get_mut::<tcp::Socket<'static>>(handle),
                super::placeholder_socket(),
            );
            Ok((Some((handle, tcp)), pending_error, peer_closed))
        },
        |(loan, pending_error, peer_closed)| {
            let Some((_, tcp)) = loan else {
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
                    false,
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
            } else if !tcp.may_recv() {
                if let Some(error) = *pending_error {
                    return Err(error);
                }
                if !*peer_closed && tcp.state() == State::Closed {
                    return Err(SocketError::ConnectionReset);
                }
                0
            } else {
                return Err(SocketError::Again);
            };
            let peer = tcp.remote_endpoint().map_or(
                InetAddress {
                    address: Ipv4Addr::UNSPECIFIED,
                    port: 0,
                },
                |endpoint| InetAddress {
                    address: from_ip(endpoint.addr),
                    port: endpoint.port,
                },
            );
            Ok((
                (count, count, peer, None),
                tcp.can_recv() || !tcp.may_recv(),
            ))
        },
        |network, (loan, _, _)| {
            if let Some((handle, tcp)) = loan {
                let placeholder = core::mem::replace(
                    network.sockets.get_mut::<tcp::Socket<'static>>(handle),
                    tcp,
                );
                debug_assert_eq!(placeholder.state(), State::Closed);
            }
        },
    )?;
    if !peek && !still_readable {
        socket.consume_notify();
    }
    Ok(result)
}

/// @description 从唯一 TCP endpoint state 投影 OFD readiness。
/// @param socket TCP facade identity。
/// @return listener/connect/connected 对应的 poll state。
/// @errors endpoint 不可用时返回 error readiness。
pub(in crate::socket::inet) fn poll_state(socket: &InetSocket) -> SocketPollState {
    let Ok(stack) = stack() else {
        return SocketPollState::error();
    };
    let Ok(network) = stack.lock() else {
        return SocketPollState::error();
    };
    network
        .tcp_endpoints
        .get(&endpoint_id(socket))
        .map_or(SocketPollState::error(), |state| state.poll_state(&network))
}

impl TcpEndpointState {
    /// @description 在已持有 NetworkStack lock 时计算 endpoint readiness。
    /// @param network 唯一协议栈 owner。
    /// @return 不注册 waiter 的状态快照。
    /// @errors 无错误。
    pub(in crate::socket::inet) fn poll_state(&self, network: &NetworkStack) -> SocketPollState {
        match self.mode {
            TcpMode::Fresh { .. } => SocketPollState {
                readable: false,
                writable: false,
                hangup: false,
                error: false,
            },
            TcpMode::Listening { .. } => SocketPollState {
                readable: self.handles.iter().any(|handle| {
                    matches!(
                        network.sockets.get::<tcp::Socket<'static>>(*handle).state(),
                        State::Established | State::CloseWait
                    )
                }),
                writable: false,
                hangup: false,
                error: false,
            },
            TcpMode::Connecting => {
                let state = network
                    .sockets
                    .get::<tcp::Socket<'static>>(self.handles[0])
                    .state();
                SocketPollState {
                    readable: state == State::Closed,
                    writable: matches!(state, State::Established | State::Closed),
                    hangup: state == State::Closed,
                    error: state == State::Closed || self.pending_error.is_some(),
                }
            }
            TcpMode::Connected { shutdown_read, .. } => {
                let socket = network.sockets.get::<tcp::Socket<'static>>(self.handles[0]);
                let closed = socket.state() == State::Closed;
                SocketPollState {
                    readable: shutdown_read || socket.can_recv() || !socket.may_recv(),
                    writable: socket.can_send(),
                    hangup: !socket.may_recv(),
                    error: closed && self.pending_error.is_some(),
                }
            }
        }
    }
}

/// @description 在协议 poll 内提交 connect/FIN/reset 状态。
/// @param network 唯一协议栈 owner。
/// @return 无返回值。
/// @errors 状态不变量破坏时 fail-stop。
pub(in crate::socket::inet) fn maintain(network: &mut NetworkStack) {
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
            TcpMode::Connecting => {
                let tcp = sockets.get::<tcp::Socket<'static>>(state.handles[0]);
                match tcp.state() {
                    State::Established => {
                        state.mode = TcpMode::Connected {
                            peer_closed: false,
                            shutdown_read: false,
                        };
                    }
                    State::Closed => {
                        state
                            .pending_error
                            .get_or_insert(SocketError::ConnectionRefused);
                    }
                    _ => {}
                }
            }
            TcpMode::Connected {
                peer_closed,
                shutdown_read,
            } => {
                let tcp = sockets.get_mut::<tcp::Socket<'static>>(state.handles[0]);
                if matches!(
                    tcp.state(),
                    State::CloseWait | State::LastAck | State::TimeWait
                ) {
                    *peer_closed = true;
                }
                if tcp.state() == State::Closed && !*peer_closed {
                    state
                        .pending_error
                        .get_or_insert(SocketError::ConnectionReset);
                }
                if *shutdown_read {
                    while tcp.can_recv() {
                        let _ = tcp.recv(|bytes| (bytes.len(), ()));
                    }
                }
            }
            TcpMode::Fresh { .. } | TcpMode::Listening { .. } => {}
        }
    });
}

/// @description egress 已观察 FIN/reset 后回收 Closed orphan 及其 socket handles。
pub(in crate::socket::inet) fn reap_orphans(network: &mut NetworkStack) {
    let NetworkStack {
        tcp_endpoints,
        sockets,
        tcp_ports,
        ..
    } = network;
    tcp_endpoints.retain(|_, state| {
        if !state.orphaned
            || state
                .handles
                .iter()
                .any(|handle| sockets.get::<tcp::Socket<'static>>(*handle).state() != State::Closed)
        {
            return true;
        }
        for &handle in &state.handles {
            sockets.remove(handle);
        }
        if let Some(lease) = state.port_lease {
            tcp_ports.release(lease);
        }
        false
    });
}

/// @description 原子读取并清除 TCP pending error，供 `SO_ERROR` 使用。
/// @param socket TCP facade identity。
/// @return 尚未消费的错误；没有时为 None。
/// @errors stack/endpoint 不可用时按无 pending error 处理。
pub(in crate::socket::inet) fn take_error(socket: &InetSocket) -> Option<SocketError> {
    stack()
        .ok()?
        .lock()
        .ok()?
        .tcp_endpoints
        .get_mut(&endpoint_id(socket))?
        .pending_error
        .take()
}

/// @description 按 Linux `SHUT_RD/WR/RDWR` 更新 TCP half-close 状态。
/// @param socket TCP facade identity。
/// @param how 0、1 或 2；syscall 层已完成范围校验。
/// @return 成功提交 half-close 返回 unit。
/// @errors 非 connected endpoint 返回 `NotConnected`。
pub(in crate::socket::inet) fn shutdown(
    socket: &InetSocket,
    how: usize,
) -> Result<(), SocketError> {
    let id = endpoint_id(socket);
    let mut network = stack()?.lock()?;
    let state = network
        .tcp_endpoints
        .get_mut(&id)
        .ok_or(SocketError::NotConnected)?;
    let TcpMode::Connected { shutdown_read, .. } = &mut state.mode else {
        return Err(SocketError::NotConnected);
    };
    if matches!(how, 0 | 2) {
        *shutdown_read = true;
    }
    if matches!(how, 1 | 2) {
        let handle = state.handles[0];
        network
            .sockets
            .get_mut::<tcp::Socket<'static>>(handle)
            .close();
    }
    drop(network);
    if matches!(how, 1 | 2) {
        crate::drivers::network::request_poll();
    }
    Ok(())
}
