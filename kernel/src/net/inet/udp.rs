use alloc::{sync::Weak, vec::Vec};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::udp,
    wire::{IpEndpoint, IpListenEndpoint},
};

use super::{
    EndpointState, InetSocket, InetSocketOptions, NetworkStack, from_ip, ipv4, port_error,
};
use crate::{
    fallible_tree::FallibleMap,
    ipc::ReceiveBuffer,
    net::{InetAddress, InternetPacketInfo, InternetSendPacketInfo, SocketError, SocketPollState},
};

// Linux SOCK_BINDADDR_LOCK/SOCK_BINDPORT_LOCK；只冻结用户 bind policy，不另存 tuple。
const BIND_ADDRESS: u8 = 1;
const BIND_PORT: u8 = 2;
const RECEIVE_SHUTDOWN: u8 = 1;
const SEND_SHUTDOWN: u8 = 2;
const UDP_PACKET_SLOTS: usize = 8;
const UDP_BUFFER_BYTES: usize = 128 * 1024;

fn placeholder_socket() -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(Vec::new(), Vec::new()),
        udp::PacketBuffer::new(Vec::new(), Vec::new()),
    )
}

/// 分配 UDP packet buffer，并在唯一 NetworkStack 注册 handle。
///
/// # Parameters
///
/// - `network`: 唯一协议栈 owner。
/// - `endpoint`: 完整 Arc 发布前的弱引用占位。
///
/// # Returns
///
/// 已注册的 smoltcp handle。
///
/// # Errors
///
/// buffer 分配失败返回 `NoMemory`。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
) -> Result<SocketHandle, SocketError> {
    let endpoint_slot = FallibleMap::<SocketHandle, EndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    let mut rx_metadata = Vec::new();
    rx_metadata
        .try_reserve_exact(UDP_PACKET_SLOTS)
        .map_err(|_| SocketError::NoMemory)?;
    rx_metadata.resize(UDP_PACKET_SLOTS, udp::PacketMetadata::EMPTY);
    let mut tx_metadata = Vec::new();
    tx_metadata
        .try_reserve_exact(UDP_PACKET_SLOTS)
        .map_err(|_| SocketError::NoMemory)?;
    tx_metadata.extend_from_slice(&rx_metadata);
    let mut rx_payload = Vec::new();
    rx_payload
        .try_reserve_exact(UDP_BUFFER_BYTES)
        .map_err(|_| SocketError::NoMemory)?;
    rx_payload.resize(UDP_BUFFER_BYTES, 0);
    let mut tx_payload = Vec::new();
    tx_payload
        .try_reserve_exact(UDP_BUFFER_BYTES)
        .map_err(|_| SocketError::NoMemory)?;
    tx_payload.resize(UDP_BUFFER_BYTES, 0);
    let socket = udp::Socket::new(
        udp::PacketBuffer::new(rx_metadata, rx_payload),
        udp::PacketBuffer::new(tx_metadata, tx_payload),
    );
    let handle = network.add_socket(socket)?;
    network.endpoints.commit_vacant(endpoint_slot.fill(
        handle,
        EndpointState {
            endpoint,
            user_bind_locks: 0,
            shutdown: 0,
            options: InetSocketOptions::default(),
            port_lease: None,
            readiness: SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Ok(handle)
}

impl NetworkStack {
    fn ensure_udp_bound(&mut self, handle: SocketHandle) -> Result<(), SocketError> {
        if self.sockets.get::<udp::Socket<'static>>(handle).is_open() {
            return Ok(());
        }
        let reuse_address = self.endpoints[&handle].options.reuse_address;
        let local = self
            .sockets
            .get::<udp::Socket<'static>>(handle)
            .endpoint()
            .addr;
        let lease = self
            .udp_ports
            .acquire_ephemeral(local.map(from_ip), reuse_address)
            .map_err(port_error)?;
        if self
            .sockets
            .get_mut::<udp::Socket<'static>>(handle)
            .bind(IpListenEndpoint {
                addr: local,
                port: lease.port(),
            })
            .is_err()
        {
            self.udp_ports.release(lease);
            return Err(SocketError::AddressNotAvailable);
        }
        self.endpoints
            .get_mut(&handle)
            .expect("implicitly bound UDP endpoint disappeared")
            .port_lease = Some(lease);
        Ok(())
    }
}

/// 绑定 UDP 本地地址；port 0 经唯一 allocator 分配 ephemeral port。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
/// - `address`: 请求的本地 IPv4 endpoint。
///
/// # Returns
///
/// 成功返回 unit。
///
/// # Errors
///
/// 返回状态、地址、冲突或分配错误。
pub(super) fn bind(
    endpoint: &InetSocket,
    handle: SocketHandle,
    address: InetAddress,
) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    if network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .is_open()
    {
        return Err(SocketError::Invalid);
    }
    let address_filter = (!address.address.is_unspecified()).then_some(address.address);
    if address_filter.is_some_and(|candidate| !network.has_local_address(candidate)) {
        return Err(SocketError::AddressNotAvailable);
    }
    let reuse_address = network.endpoints[&handle].options.reuse_address;
    let lease = if address.port == 0 {
        network
            .udp_ports
            .acquire_ephemeral(address_filter, reuse_address)
            .map_err(port_error)?
    } else {
        network
            .udp_ports
            .acquire(address.port, address_filter, reuse_address)
            .map_err(port_error)?
    };
    if network
        .sockets
        .get_mut::<udp::Socket<'static>>(handle)
        .bind(IpListenEndpoint {
            addr: address_filter.map(ipv4),
            port: lease.port(),
        })
        .is_err()
    {
        network.udp_ports.release(lease);
        return Err(SocketError::AddressNotAvailable);
    }
    let state = network
        .endpoints
        .get_mut(&handle)
        .expect("bound UDP endpoint disappeared");
    state.port_lease = Some(lease);
    state.user_bind_locks |= if address_filter.is_some() {
        BIND_ADDRESS
    } else {
        0
    } | if address.port != 0 { BIND_PORT } else { 0 };
    Ok(())
}

/// 为 UDP endpoint 记录默认 peer，并按需完成隐式 bind。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
/// - `peer`: 默认远端 endpoint。
///
/// # Returns
///
/// 成功返回 unit。
///
/// # Errors
///
/// 远端无效、端口耗尽或 endpoint 消失时返回错误。
pub(super) fn connect(
    endpoint: &InetSocket,
    handle: SocketHandle,
    peer: InetAddress,
) -> Result<(), SocketError> {
    if peer.port == 0 || peer.address.is_unspecified() {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = endpoint.stack()?.lock()?;
    network.ensure_udp_bound(handle)?;
    let route = network.route(
        peer.address,
        network.endpoints[&handle].options.bound_to_device,
    )?;
    let local_address = network.source_address(route).map(ipv4);
    let socket = network.sockets.get_mut::<udp::Socket<'static>>(handle);
    let mut local = socket.endpoint();
    if local.addr.is_none() {
        local.addr = local_address;
    }
    socket.set_local_endpoint(local);
    socket.set_peer_endpoint(Some(IpEndpoint::new(ipv4(peer.address), peer.port)));
    Ok(())
}

/// 读取 UDP handle 的权威本地 endpoint。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
///
/// # Returns
///
/// 本地地址；未 bind 时为 unspecified:0。
///
/// # Errors
///
/// NetworkStack 未初始化时返回错误。
pub(super) fn address(
    endpoint: &InetSocket,
    handle: SocketHandle,
) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    let endpoint = network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .endpoint();
    Ok(InetAddress {
        address: endpoint.addr.map(from_ip).unwrap_or(Ipv4Addr::UNSPECIFIED),
        port: endpoint.port,
    })
}

/// 读取 connected UDP 的默认 peer。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
///
/// # Returns
///
/// 默认远端 endpoint。
///
/// # Errors
///
/// 未 connect 或 endpoint 已删除时返回 `NotConnected`。
pub(super) fn peer_address(
    endpoint: &InetSocket,
    handle: SocketHandle,
) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .peer_endpoint()
        .map(|peer| InetAddress {
            address: from_ip(peer.addr),
            port: peer.port,
        })
        .ok_or(SocketError::NotConnected)
}

/// 向显式目标或 connected peer 原子排队一个 UDP datagram。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
/// - `input`: 完整 datagram payload。
/// - `target`: 可选显式远端 endpoint。
///
/// # Returns
///
/// 成功排队的完整 payload 长度。
///
/// # Errors
///
/// 返回 datagram 大小、route、地址、buffer 或 endpoint 错误。
pub(super) fn send(
    endpoint: &InetSocket,
    handle: SocketHandle,
    input: &[u8],
    target: Option<InetAddress>,
    packet_info: Option<InternetSendPacketInfo>,
) -> Result<usize, SocketError> {
    if input.len() > crate::net::message_limits::MAX_IPV4_UDP_BYTES {
        return Err(SocketError::MessageTooLarge);
    }
    let stack = endpoint.stack()?;
    let written = stack.with_payload_loan(
        |network| {
            network.ensure_udp_bound(handle)?;
            let peer = target
                .or_else(|| {
                    network
                        .sockets
                        .get::<udp::Socket<'static>>(handle)
                        .peer_endpoint()
                        .map(|peer| InetAddress {
                            address: from_ip(peer.addr),
                            port: peer.port,
                        })
                })
                .ok_or(SocketError::DestinationRequired)?;
            if peer.port == 0 || peer.address.is_unspecified() {
                return Err(SocketError::AddressNotAvailable);
            }
            let options = network.endpoints[&handle].options;
            let outgoing = packet_info
                .and_then(|info| info.interface_index)
                .or(options.bound_to_device);
            if let Some(index) = outgoing {
                network.interface(index)?;
            }
            let requested_source = packet_info.and_then(|info| info.source_address);
            if requested_source.is_some_and(|source| !network.has_local_address(source)) {
                return Err(SocketError::NetworkUnreachable);
            }
            let route = network.route(peer.address, outgoing)?;
            // 未开放 IPv4 fragmentation；按真实本次出口 MTU 在入队前拒绝，不能成功后丢包。
            if input.len() > route.transport.mtu.saturating_sub(20 + 8) {
                return Err(SocketError::MessageTooLarge);
            }
            let interface_index = route.logical_interface_index;
            let route_address = network.source_address(route);
            if route.kind == super::route::Ipv4RouteKind::Broadcast && !options.broadcast {
                return Err(SocketError::PermissionDenied);
            }
            if network.endpoints[&handle].shutdown & SEND_SHUTDOWN != 0 {
                return Err(SocketError::BrokenPipe);
            }
            let bound_source = network
                .sockets
                .get::<udp::Socket<'static>>(handle)
                .endpoint()
                .addr;
            // cmsg明确给出0源地址时清除本次request的bound source，实际binding保持。
            let local = if packet_info.is_some() {
                requested_source
                    .map(ipv4)
                    .or_else(|| route_address.map(ipv4))
            } else {
                bound_source.or_else(|| route_address.map(ipv4))
            };
            let socket = core::mem::replace(
                network.sockets.get_mut::<udp::Socket<'static>>(handle),
                placeholder_socket(),
            );
            Ok((socket, peer, local, interface_index))
        },
        |(socket, peer, local, interface_index)| {
            let mut metadata: udp::UdpMetadata =
                IpEndpoint::new(ipv4(peer.address), peer.port).into();
            metadata.local_address = *local;
            metadata.interface_index = Some(*interface_index);
            socket
                .send_slice(input, metadata)
                .map(|()| input.len())
                .map_err(|error| match error {
                    udp::SendError::BufferFull => SocketError::Again,
                    udp::SendError::Unaddressable => SocketError::NetworkUnreachable,
                })
        },
        |network, (socket, _, _, _)| {
            let placeholder = core::mem::replace(
                network.sockets.get_mut::<udp::Socket<'static>>(handle),
                socket,
            );
            debug_assert!(!placeholder.is_open());
        },
    )?;
    crate::devices::model::network::request_poll();
    Ok(written)
}

/// 接收或窥视一个 UDP datagram，并保留原始 datagram 长度。
///
/// # Parameters
///
/// - `endpoint`: OFD-facing endpoint，用于消费 readiness notification。
/// - `handle`: UDP smoltcp handle。
/// - `output`: kernel-owned 输出缓冲区。
/// - `peek`: 为 true 时不消费 datagram。
///
/// # Returns
///
/// copied/full length、source 与 local destination。
///
/// # Errors
///
/// 无可用 datagram 时返回 `Again`。
pub(super) fn receive(
    endpoint: &InetSocket,
    handle: SocketHandle,
    output: &mut ReceiveBuffer<'_>,
    peek: bool,
) -> Result<
    (
        usize,
        usize,
        Option<InetAddress>,
        Option<InternetPacketInfo>,
    ),
    SocketError,
> {
    let stack = endpoint.stack()?;
    let (result, drained) = stack.with_payload_loan(
        |network| {
            let read_closed = network.endpoints[&handle].shutdown & RECEIVE_SHUTDOWN != 0;
            Ok((
                core::mem::replace(
                    network.sockets.get_mut::<udp::Socket<'static>>(handle),
                    placeholder_socket(),
                ),
                read_closed,
            ))
        },
        |(socket, read_closed)| {
            // UDP shutdown 不清队列也不拒绝之后的新 ingress；仅在实际 queue 空时返回 EOF。
            if !socket.can_recv() && *read_closed {
                return Ok(((0, 0, None, None), true));
            }
            let received = if peek {
                socket
                    .peek()
                    .map(|(payload, metadata)| (payload, *metadata))
            } else {
                socket.recv()
            };
            let result = received
                .map(|(payload, metadata)| {
                    let full_length = payload.len();
                    let count = output.append(payload);
                    (
                        count,
                        full_length,
                        Some(InetAddress {
                            address: from_ip(metadata.endpoint.addr),
                            port: metadata.endpoint.port,
                        }),
                        match (
                            metadata.interface_index,
                            metadata.local_spec_address,
                            metadata.local_address,
                        ) {
                            (Some(interface_index), Some(specific_address), Some(destination)) => {
                                Some(InternetPacketInfo {
                                    interface_index,
                                    specific_address: from_ip(specific_address),
                                    destination: from_ip(destination),
                                })
                            }
                            _ => None,
                        },
                    )
                })
                .map_err(|_| SocketError::Again)?;
            Ok((result, !peek && !socket.can_recv()))
        },
        |network, (socket, _)| {
            let placeholder = core::mem::replace(
                network.sockets.get_mut::<udp::Socket<'static>>(handle),
                socket,
            );
            debug_assert!(!placeholder.is_open());
        },
    )?;
    if drained {
        endpoint.consume_notify();
    }
    Ok(result)
}

/// 设置 UDP `IP_PKTINFO` ancillary 投影开关。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
/// - `enabled`: 是否在 recvmsg 生成 pktinfo。
///
/// # Returns
///
/// endpoint state 已更新时返回 unit。
///
/// # Errors
///
/// stack/endpoint 不可用或 owner waiter OOM 时返回对应 socket error。
/// 从唯一 UDP handle 投影 OFD readiness。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
///
/// # Returns
///
/// readable/writable/error/hangup 状态。
///
/// # Errors
///
/// stack 不可用时返回 error readiness。
pub(super) fn poll_state(endpoint: &InetSocket, handle: SocketHandle) -> SocketPollState {
    let Ok(stack) = endpoint.stack() else {
        return SocketPollState::error();
    };
    let Ok(network) = stack.lock() else {
        return SocketPollState::error();
    };
    let socket = network.sockets.get::<udp::Socket<'static>>(handle);
    poll_state_locked(socket, &network.endpoints[&handle])
}

/// 在 deferred source 通知中无等待地投影 UDP readiness。
///
/// # Parameters
///
/// - `handle`: UDP smoltcp handle。
///
/// # Returns
///
/// owner 可立即观察且 SocketSet 完整时返回状态，否则返回 `None`。
///
/// # Errors
///
/// 只投影实际endpoint状态；设备诊断不生成socket协议错误。
pub(super) fn try_poll_state(
    endpoint: &InetSocket,
    handle: SocketHandle,
) -> Option<SocketPollState> {
    let network = endpoint.try_observe_stack()?;
    let socket = network.sockets.get::<udp::Socket<'static>>(handle);
    Some(poll_state_locked(socket, &network.endpoints[&handle]))
}

/// 删除 UDP metadata 与同一个 smoltcp handle。
///
/// # Parameters
///
/// - `network`: 已由 protocol owner 独占的完整 NetworkStack。
/// - `handle`: UDP smoltcp handle。
///
/// # Errors
///
/// 重复删除或 stack 未初始化时幂等忽略。
pub(super) fn drop_endpoint(network: &mut NetworkStack, handle: SocketHandle) {
    if let Some(state) = network.endpoints.remove(&handle) {
        if let Some(lease) = state.port_lease {
            network.udp_ports.release(lease);
        }
        network.sockets.remove(handle);
    }
}

/// native UDP AF_UNSPEC：取消关联与未锁定的实际本地绑定，既有 RX/TX 数据保持。
pub(super) fn disconnect(endpoint: &InetSocket, handle: SocketHandle) -> Result<(), SocketError> {
    let mut network = endpoint.stack()?.lock()?;
    let locks = network.endpoints[&handle].user_bind_locks;
    let socket = network.sockets.get_mut::<udp::Socket<'static>>(handle);
    let mut local = socket.endpoint();
    socket.set_peer_endpoint(None);
    socket.set_bound_interface(None);
    if locks & BIND_ADDRESS == 0 {
        local.addr = None;
    }
    if locks & BIND_PORT == 0 {
        local.port = 0;
    }
    socket.set_local_endpoint(local);
    let state = network
        .endpoints
        .get_mut(&handle)
        .expect("live UDP metadata");
    state.options.bound_to_device = None;
    if locks & BIND_PORT == 0
        && let Some(lease) = state.port_lease.take()
    {
        network.udp_ports.release(lease);
    }
    Ok(())
}

/// 同一 sk_shutdown owner：未关联也提交位集合后返回 ENOTCONN，connect/disconnect 不重置。
pub(super) fn shutdown(
    endpoint: &InetSocket,
    handle: SocketHandle,
    how: usize,
) -> Result<(), SocketError> {
    if how > 2 {
        return Err(SocketError::Invalid);
    }
    let mut network = endpoint.stack()?.lock()?;
    let connected = network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .peer_endpoint()
        .is_some();
    network
        .endpoints
        .get_mut(&handle)
        .expect("live UDP metadata")
        .shutdown |= (how + 1) as u8;
    if connected {
        Ok(())
    } else {
        Err(SocketError::NotConnected)
    }
}

/// 所有直接/deferred/edge poll 共用同一实际 receive queue 与 native shutdown 投影。
pub(super) fn poll_state_locked(
    socket: &udp::Socket<'static>,
    state: &EndpointState,
) -> SocketPollState {
    let read_closed = state.shutdown & RECEIVE_SHUTDOWN != 0;
    SocketPollState {
        readable: socket.can_recv() || read_closed,
        writable: socket.can_send(),
        hangup: state.shutdown == RECEIVE_SHUTDOWN | SEND_SHUTDOWN,
        read_closed,
        error: false,
    }
}
