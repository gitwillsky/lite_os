use alloc::{sync::Weak, vec::Vec};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::udp,
    wire::{IpEndpoint, IpListenEndpoint},
};

use super::{
    EPHEMERAL_END, EPHEMERAL_START, EndpointState, InetSocket, NetworkStack, from_ip, ipv4, now,
    stack,
};
use crate::socket::{InetAddress, SocketError, SocketPollState};

const UDP_PACKET_SLOTS: usize = 8;
const UDP_BUFFER_BYTES: usize = 128 * 1024;
const MAX_UDP_PAYLOAD: usize = 65_507;

/// @description 分配 UDP packet buffer，并在唯一 NetworkStack 注册 handle。
/// @param network 唯一协议栈 owner。
/// @param endpoint 完整 Arc 发布前的弱引用占位。
/// @return 已注册的 smoltcp handle。
/// @errors buffer 分配失败返回 `NoMemory`。
pub(super) fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
) -> Result<SocketHandle, SocketError> {
    let mut rx_metadata = Vec::new();
    rx_metadata
        .try_reserve_exact(UDP_PACKET_SLOTS)
        .map_err(|_| SocketError::NoMemory)?;
    rx_metadata.resize(UDP_PACKET_SLOTS, udp::PacketMetadata::EMPTY);
    let tx_metadata = rx_metadata.clone();
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
    let handle = network.sockets.add(socket);
    network.endpoints.insert(
        handle,
        EndpointState {
            endpoint,
            peer: None,
            packet_info: false,
        },
    );
    Ok(handle)
}

impl NetworkStack {
    fn udp_port_in_use(&self, address: Option<Ipv4Addr>, port: u16, except: SocketHandle) -> bool {
        self.endpoints.keys().any(|handle| {
            if *handle == except {
                return false;
            }
            let endpoint = self.sockets.get::<udp::Socket<'static>>(*handle).endpoint();
            endpoint.port == port
                && (endpoint.addr.is_none()
                    || address.is_none()
                    || endpoint.addr == address.map(ipv4))
        })
    }

    fn allocate_udp_ephemeral(&mut self, handle: SocketHandle) -> Result<u16, SocketError> {
        for _ in EPHEMERAL_START..=EPHEMERAL_END {
            let candidate = self.next_ephemeral;
            self.next_ephemeral = if candidate == EPHEMERAL_END {
                EPHEMERAL_START
            } else {
                candidate + 1
            };
            if !self.udp_port_in_use(None, candidate, handle) {
                return Ok(candidate);
            }
        }
        Err(SocketError::AddressInUse)
    }

    fn ensure_udp_bound(&mut self, handle: SocketHandle) -> Result<(), SocketError> {
        if self.sockets.get::<udp::Socket<'static>>(handle).is_open() {
            return Ok(());
        }
        let port = self.allocate_udp_ephemeral(handle)?;
        self.sockets
            .get_mut::<udp::Socket<'static>>(handle)
            .bind(port)
            .map_err(|_| SocketError::AddressNotAvailable)
    }
}

/// @description 绑定 UDP 本地地址；port 0 经唯一 allocator 分配 ephemeral port。
/// @param handle UDP smoltcp handle。
/// @param address 请求的本地 IPv4 endpoint。
/// @return 成功返回 unit。
/// @errors 返回状态、地址、冲突或分配错误。
pub(super) fn bind(handle: SocketHandle, address: InetAddress) -> Result<(), SocketError> {
    let mut network = stack()?.lock();
    if network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .is_open()
    {
        return Err(SocketError::Invalid);
    }
    let address_filter = (!address.address.is_unspecified()).then_some(address.address);
    if address_filter.is_some_and(|candidate| {
        network.interface_state.address != Some(candidate) || !network.interface_state.up
    }) {
        return Err(SocketError::AddressNotAvailable);
    }
    let port = if address.port == 0 {
        network.allocate_udp_ephemeral(handle)?
    } else {
        address.port
    };
    if network.udp_port_in_use(address_filter, port, handle) {
        return Err(SocketError::AddressInUse);
    }
    network
        .sockets
        .get_mut::<udp::Socket<'static>>(handle)
        .bind(IpListenEndpoint {
            addr: address_filter.map(ipv4),
            port,
        })
        .map_err(|_| SocketError::AddressNotAvailable)
}

/// @description 为 UDP endpoint 记录默认 peer，并按需完成隐式 bind。
/// @param handle UDP smoltcp handle。
/// @param peer 默认远端 endpoint。
/// @return 成功返回 unit。
/// @errors 远端无效、端口耗尽或 endpoint 消失时返回错误。
pub(super) fn connect(handle: SocketHandle, peer: InetAddress) -> Result<(), SocketError> {
    if peer.port == 0 || peer.address.is_unspecified() {
        return Err(SocketError::AddressNotAvailable);
    }
    let mut network = stack()?.lock();
    network.ensure_udp_bound(handle)?;
    network
        .endpoints
        .get_mut(&handle)
        .expect("AF_INET UDP endpoint metadata disappeared")
        .peer = Some(peer);
    Ok(())
}

/// @description 读取 UDP handle 的权威本地 endpoint。
/// @param handle UDP smoltcp handle。
/// @return 本地地址；未 bind 时为 unspecified:0。
/// @errors NetworkStack 未初始化时返回错误。
pub(super) fn address(handle: SocketHandle) -> Result<InetAddress, SocketError> {
    let network = stack()?.lock();
    let endpoint = network
        .sockets
        .get::<udp::Socket<'static>>(handle)
        .endpoint();
    Ok(InetAddress {
        address: endpoint.addr.map(from_ip).unwrap_or(Ipv4Addr::UNSPECIFIED),
        port: endpoint.port,
    })
}

/// @description 读取 connected UDP 的默认 peer。
/// @param handle UDP smoltcp handle。
/// @return 默认远端 endpoint。
/// @errors 未 connect 或 endpoint 已删除时返回 `NotConnected`。
pub(super) fn peer_address(handle: SocketHandle) -> Result<InetAddress, SocketError> {
    stack()?
        .lock()
        .endpoints
        .get(&handle)
        .and_then(|state| state.peer)
        .ok_or(SocketError::NotConnected)
}

/// @description 向显式目标或 connected peer 原子排队一个 UDP datagram。
/// @param handle UDP smoltcp handle。
/// @param input 完整 datagram payload。
/// @param target 可选显式远端 endpoint。
/// @return 成功排队的完整 payload 长度。
/// @errors 返回 datagram 大小、route、地址、buffer 或 endpoint 错误。
pub(super) fn send(
    handle: SocketHandle,
    input: &[u8],
    target: Option<InetAddress>,
) -> Result<usize, SocketError> {
    if input.len() > MAX_UDP_PAYLOAD {
        return Err(SocketError::MessageTooLarge);
    }
    let mut network = stack()?.lock();
    if !network.interface_state.up || network.interface_state.address.is_none() {
        return Err(SocketError::NetworkUnreachable);
    }
    network.ensure_udp_bound(handle)?;
    let peer = target
        .or_else(|| network.endpoints.get(&handle).and_then(|state| state.peer))
        .ok_or(SocketError::DestinationRequired)?;
    if peer.port == 0 || peer.address.is_unspecified() {
        return Err(SocketError::AddressNotAvailable);
    }
    network
        .sockets
        .get_mut::<udp::Socket<'static>>(handle)
        .send_slice(input, IpEndpoint::new(ipv4(peer.address), peer.port))
        .map_err(|error| match error {
            udp::SendError::BufferFull => SocketError::Again,
            udp::SendError::Unaddressable => SocketError::NetworkUnreachable,
        })?;
    let NetworkStack {
        interface,
        device,
        sockets,
        ..
    } = &mut *network;
    interface.poll_egress(now(), device, sockets);
    Ok(input.len())
}

/// @description 接收或窥视一个 UDP datagram，并保留原始 datagram 长度。
/// @param endpoint OFD-facing endpoint，用于消费 readiness notification。
/// @param handle UDP smoltcp handle。
/// @param output kernel-owned 输出缓冲区。
/// @param peek 为 true 时不消费 datagram。
/// @return copied/full length、source 与 local destination。
/// @errors 无可用 datagram 时返回 `Again`。
pub(super) fn receive(
    endpoint: &InetSocket,
    handle: SocketHandle,
    output: &mut [u8],
    peek: bool,
) -> Result<(usize, usize, InetAddress, Option<Ipv4Addr>), SocketError> {
    let mut network = stack()?.lock();
    let socket = network.sockets.get_mut::<udp::Socket<'static>>(handle);
    let received = if peek {
        socket
            .peek()
            .map(|(payload, metadata)| (payload, *metadata))
    } else {
        socket.recv()
    };
    let result = received.map(|(payload, metadata)| {
        let full_length = payload.len();
        let count = output.len().min(full_length);
        output[..count].copy_from_slice(&payload[..count]);
        (
            count,
            full_length,
            InetAddress {
                address: from_ip(metadata.endpoint.addr),
                port: metadata.endpoint.port,
            },
            metadata.local_address.map(from_ip),
        )
    });
    let drained = result.is_ok() && !peek && !socket.can_recv();
    drop(network);
    let result = result.map_err(|_| SocketError::Again)?;
    if drained {
        endpoint.consume_notify();
    }
    Ok(result)
}

/// @description 设置 UDP `IP_PKTINFO` ancillary 投影开关。
/// @param handle UDP smoltcp handle。
/// @param enabled 是否在 recvmsg 生成 pktinfo。
/// @return 无返回值。
/// @errors endpoint 状态丢失表示 owner 不变量破坏并 fail-stop。
pub(super) fn set_packet_info(handle: SocketHandle, enabled: bool) {
    stack()
        .expect("AF_INET UDP endpoint lost network stack")
        .lock()
        .endpoints
        .get_mut(&handle)
        .expect("AF_INET UDP endpoint metadata disappeared")
        .packet_info = enabled;
}

/// @description 查询 UDP `IP_PKTINFO` 开关。
/// @param handle UDP smoltcp handle。
/// @return 已启用且 endpoint 存在时返回 true。
/// @errors 无错误。
pub(super) fn packet_info(handle: SocketHandle) -> bool {
    stack().is_ok_and(|stack| {
        stack
            .lock()
            .endpoints
            .get(&handle)
            .is_some_and(|state| state.packet_info)
    })
}

/// @description 从唯一 UDP handle 投影 OFD readiness。
/// @param handle UDP smoltcp handle。
/// @return readable/writable/error/hangup 状态。
/// @errors stack 不可用时返回 error readiness。
pub(super) fn poll_state(handle: SocketHandle) -> SocketPollState {
    let Ok(stack) = stack() else {
        return SocketPollState::error();
    };
    let network = stack.lock();
    let socket = network.sockets.get::<udp::Socket<'static>>(handle);
    SocketPollState {
        readable: socket.can_recv(),
        writable: socket.can_send(),
        hangup: false,
        error: false,
    }
}

/// @description 删除 UDP metadata 与同一个 smoltcp handle。
/// @param handle UDP smoltcp handle。
/// @return 无返回值。
/// @errors 重复删除或 stack 未初始化时幂等忽略。
pub(super) fn drop_endpoint(handle: SocketHandle) {
    let Some(stack) = super::NETWORK_STACK.get() else {
        return;
    };
    let mut network = stack.lock();
    if network.endpoints.remove(&handle).is_some() {
        network.sockets.remove(handle);
    }
}
