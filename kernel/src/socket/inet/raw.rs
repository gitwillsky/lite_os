use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::raw,
    wire::{IpProtocol, IpVersion, Ipv4Packet, Ipv4Repr},
};

use super::{InetEndpoint, InetSocket, NetworkStack, from_ip, ipv4, stack, try_allocate_endpoint};
use crate::{
    fallible_tree::FallibleMap,
    ipc::PipeEnd,
    ipc::ReceiveBuffer,
    socket::{
        InetAddress, InternetPacketInfo, InternetSendPacketInfo, SocketError, SocketPollState,
    },
};

const IPV4_HEADER_LENGTH: usize = 20;
const RAW_PACKET_SLOTS: usize = 8;
const RAW_PACKET_CAPACITY: usize = 2048;

fn placeholder_socket() -> raw::Socket<'static> {
    raw::Socket::new(
        Some(IpVersion::Ipv4),
        Some(IpProtocol::Icmp),
        raw::PacketBuffer::new(Vec::new(), Vec::new()),
        raw::PacketBuffer::new(Vec::new(), Vec::new()),
    )
}

pub(super) struct RawEndpointState {
    pub(super) endpoint: Weak<InetSocket>,
    pub(super) options: super::InetSocketOptions,
    // OWNER: endpoint 不可变的 native protocol；IP_HDRINCL 是独立、可变的输入 codec policy。
    pub(super) protocol: u8,
    // 协议 poll 前的唯一 edge 快照；缺失时长期 writable 会制造重复 wake。
    pub(super) readiness: SocketPollState,
    // 只跨越 stack unlock 保存一次 transition；缺失会在持 stack lock 时通知并形成反向锁序。
    pub(super) notification_pending: bool,
}

fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
    protocol: u8,
) -> Result<SocketHandle, SocketError> {
    let endpoint_slot = FallibleMap::<SocketHandle, RawEndpointState>::try_reserve_node()
        .map_err(|_| SocketError::NoMemory)?;
    fn metadata() -> Result<Vec<raw::PacketMetadata>, SocketError> {
        let mut metadata = Vec::new();
        metadata
            .try_reserve_exact(RAW_PACKET_SLOTS)
            .map_err(|_| SocketError::NoMemory)?;
        metadata.resize(RAW_PACKET_SLOTS, raw::PacketMetadata::EMPTY);
        Ok(metadata)
    }

    fn payload() -> Result<Vec<u8>, SocketError> {
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(RAW_PACKET_SLOTS * RAW_PACKET_CAPACITY)
            .map_err(|_| SocketError::NoMemory)?;
        payload.resize(RAW_PACKET_SLOTS * RAW_PACKET_CAPACITY, 0);
        Ok(payload)
    }

    let rx = raw::PacketBuffer::new(metadata()?, payload()?);
    let tx = raw::PacketBuffer::new(metadata()?, payload()?);
    let mut socket = raw::Socket::new(
        Some(IpVersion::Ipv4),
        Some(IpProtocol::from(protocol)),
        rx,
        tx,
    );
    // RAW255 是 Linux send-only socket；HDRINCL 可让真实 IP header 选择 protocol，
    // 普通模式仍由下面唯一 codec 填 endpoint protocol，不用 wildcard receive 伪装 RAW。
    socket.set_receive_enabled(protocol != 255);
    socket.set_transmit_any_protocol(true);
    let handle = network.add_socket(socket)?;
    network.raw_endpoints.commit_vacant(endpoint_slot.fill(
        handle,
        RawEndpointState {
            endpoint,
            options: super::InetSocketOptions {
                header_included: protocol == 255,
                ..super::InetSocketOptions::default()
            },
            protocol,
            readiness: SocketPollState::error(),
            notification_pending: false,
        },
    ));
    Ok(handle)
}

pub(super) fn new(
    protocol: u8,
    notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    timeouts: Arc<crate::socket::SocketTimeouts>,
    memory: Arc<crate::socket::SocketMemory>,
) -> Result<Arc<InetSocket>, SocketError> {
    let stack = stack(memory.namespace_ref())?;
    let mut network = stack.lock()?;
    let endpoint = try_allocate_endpoint(|| {
        let handle = create_endpoint(&mut network, Weak::new(), protocol)?;
        Ok(InetSocket {
            memory: memory.clone(),
            endpoint: InetEndpoint::Raw(handle),
            operation: spin::Mutex::new(Some(InetEndpoint::Raw(handle))),
            notify_read: notify.0,
            notify_write: notify.1,
            timeouts,
        })
    });
    let endpoint = endpoint?;
    let InetEndpoint::Raw(handle) = endpoint.endpoint else {
        unreachable!("raw constructor returned a non-raw endpoint")
    };
    network
        .raw_endpoints
        .get_mut(&handle)
        .expect("new raw endpoint disappeared before Arc publication")
        .endpoint = Arc::downgrade(&endpoint);
    Ok(endpoint)
}

pub(super) fn poll_state_locked(
    sockets: &smoltcp::iface::SocketSet<'static>,
    handle: SocketHandle,
) -> SocketPollState {
    let socket = sockets.get::<raw::Socket<'static>>(handle);
    SocketPollState {
        readable: socket.can_recv(),
        writable: socket.can_send(),
        hangup: false,
        read_closed: false,
        error: false,
    }
}

pub(super) fn bind(
    endpoint: &InetSocket,
    handle: SocketHandle,
    address: InetAddress,
) -> Result<(), SocketError> {
    if address.port != 0 {
        return Err(SocketError::Invalid);
    }
    let mut network = endpoint.stack()?.lock()?;
    if !address.address.is_unspecified() && !network.has_local_address(address.address) {
        return Err(SocketError::AddressNotAvailable);
    }
    network
        .sockets
        .get_mut::<raw::Socket<'static>>(handle)
        .set_local_address((!address.address.is_unspecified()).then_some(ipv4(address.address)));
    Ok(())
}

pub(super) fn address(
    endpoint: &InetSocket,
    handle: SocketHandle,
) -> Result<InetAddress, SocketError> {
    let network = endpoint.stack()?.lock()?;
    Ok(InetAddress {
        address: network
            .sockets
            .get::<raw::Socket<'static>>(handle)
            .local_address()
            .map(from_ip)
            .unwrap_or(Ipv4Addr::UNSPECIFIED),
        port: 0,
    })
}

pub(super) fn send(
    endpoint: &InetSocket,
    handle: SocketHandle,
    input: &[u8],
    target: Option<InetAddress>,
    packet_info: Option<InternetSendPacketInfo>,
) -> Result<usize, SocketError> {
    let target = target.ok_or(SocketError::DestinationRequired)?;
    let stack = endpoint.stack()?;
    let mut network = stack.lock()?;
    let state = network
        .raw_endpoints
        .get(&handle)
        .ok_or(SocketError::NotConnected)?;
    let options = state.options;
    let protocol = state.protocol;
    let header_included = options.header_included;
    let total_length = if header_included {
        if input.len() < IPV4_HEADER_LENGTH || input[0] >> 4 != 4 || input[0] & 15 < 5 {
            return Err(SocketError::Invalid);
        }
        // 本轮只支持基础 20-byte、未分片 IPv4 header。不能接受后再重编码丢失 options/fragments。
        if input[0] & 15 != 5 {
            return Err(SocketError::OperationNotSupported);
        }
        let fragment = u16::from_be_bytes([input[6], input[7]]);
        if fragment & 0x8000 != 0 {
            return Err(SocketError::Invalid);
        }
        if fragment & 0x3fff != 0 {
            return Err(SocketError::OperationNotSupported);
        }
        let destination = Ipv4Addr::new(input[16], input[17], input[18], input[19]);
        if destination.is_unspecified() {
            return Err(SocketError::Invalid);
        }
        // 唯一 FIB 按实际 header 目的路由；目前不开放 sockaddr/header 不同的 source-route 形态。
        if destination != target.address {
            return Err(SocketError::OperationNotSupported);
        }
        input.len()
    } else {
        input
            .len()
            .checked_add(IPV4_HEADER_LENGTH)
            .ok_or(SocketError::MessageTooLarge)?
    };
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
    let route = network.route(target.address, outgoing)?;
    let packet_capacity = network
        .sockets
        .get::<raw::Socket<'static>>(handle)
        .payload_send_capacity();
    if total_length
        > route
            .transport
            .mtu
            .min(packet_capacity)
            .min(u16::MAX as usize)
    {
        return Err(SocketError::MessageTooLarge);
    }
    let interface_index = route.logical_interface_index;
    let source = if packet_info.is_some() {
        requested_source
    } else {
        network
            .sockets
            .get::<raw::Socket<'static>>(handle)
            .local_address()
            .map(from_ip)
    }
    .or(network.source_address(route))
    .ok_or(SocketError::NetworkUnreachable)?;
    if route.kind == super::route::Ipv4RouteKind::Broadcast && !options.broadcast {
        return Err(SocketError::PermissionDenied);
    }
    let needs_id = !header_included || input[4..6] == [0, 0];
    let id = network.raw_packet_id;
    if needs_id {
        network.raw_packet_id = id.wrapping_add(1).max(1);
    }
    drop(network);
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(total_length)
        .map_err(|_| SocketError::NoMemory)?;
    if header_included {
        packet.extend_from_slice(input);
        if packet[12..16] == [0, 0, 0, 0] {
            packet[12..16].copy_from_slice(&source.octets());
        }
    } else {
        packet.resize(total_length, 0);
        packet[0] = 0x45;
        packet[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
        packet[8] = options.hop_limit.unwrap_or(super::DEFAULT_IPV4_HOP_LIMIT);
        packet[9] = protocol;
        packet[12..16].copy_from_slice(&source.octets());
        packet[16..20].copy_from_slice(&target.address.octets());
        packet[IPV4_HEADER_LENGTH..].copy_from_slice(input);
    }
    // Native raw_send_hdrinc 总是填 total length/checksum；只在用户置零时填 source/ID。
    packet[2..4].copy_from_slice(&(total_length as u16).to_be_bytes());
    if needs_id {
        packet[4..6].copy_from_slice(&id.to_be_bytes());
    }
    let mut ip = Ipv4Packet::new_unchecked(packet.as_mut_slice());
    ip.fill_checksum();
    Ipv4Repr::parse(
        &Ipv4Packet::new_unchecked(packet.as_slice()),
        &smoltcp::phy::ChecksumCapabilities::default(),
    )
    .map_err(|_| SocketError::Invalid)?;
    stack.with_payload_loan(
        |network| {
            Ok(core::mem::replace(
                network.sockets.get_mut::<raw::Socket<'static>>(handle),
                placeholder_socket(),
            ))
        },
        |socket| {
            socket.set_transmit_interface(Some(interface_index));
            socket.send_slice(&packet).map_err(|_| SocketError::Again)
        },
        |network, socket| {
            let placeholder = core::mem::replace(
                network.sockets.get_mut::<raw::Socket<'static>>(handle),
                socket,
            );
            debug_assert!(!placeholder.can_send());
        },
    )?;
    crate::drivers::network::request_poll();
    Ok(input.len())
}

pub(super) fn receive(
    endpoint: &InetSocket,
    handle: SocketHandle,
    output: &mut ReceiveBuffer<'_>,
    peek: bool,
) -> Result<(usize, usize, InetAddress, InternetPacketInfo), SocketError> {
    let stack = endpoint.stack()?;
    let (result, drained) = stack.with_payload_loan(
        |network| {
            Ok(core::mem::replace(
                network.sockets.get_mut::<raw::Socket<'static>>(handle),
                placeholder_socket(),
            ))
        },
        |socket| {
            let metadata = socket.receive_metadata().map_err(|_| SocketError::Again)?;
            let packet =
                if peek { socket.peek() } else { socket.recv() }.map_err(|_| SocketError::Again)?;
            if packet.len() < IPV4_HEADER_LENGTH {
                return Err(SocketError::Invalid);
            }
            let full_length = packet.len();
            let count = output.append(packet);
            let source = InetAddress {
                address: Ipv4Addr::from(<[u8; 4]>::try_from(&packet[12..16]).unwrap()),
                port: 0,
            };
            let local = InternetPacketInfo {
                interface_index: metadata.interface_index.ok_or(SocketError::Invalid)?,
                specific_address: from_ip(metadata.local_spec_address.ok_or(SocketError::Invalid)?),
                destination: Ipv4Addr::from(<[u8; 4]>::try_from(&packet[16..20]).unwrap()),
            };
            Ok((
                (count, full_length, source, local),
                !peek && !socket.can_recv(),
            ))
        },
        |network, socket| {
            let placeholder = core::mem::replace(
                network.sockets.get_mut::<raw::Socket<'static>>(handle),
                socket,
            );
            debug_assert!(!placeholder.can_recv());
        },
    )?;
    if drained {
        endpoint.consume_notify();
    }
    Ok(result)
}

pub(super) fn poll_state(endpoint: &InetSocket, handle: SocketHandle) -> SocketPollState {
    let Ok(network) = endpoint.stack() else {
        return SocketPollState::error();
    };
    let Ok(network) = network.lock() else {
        return SocketPollState::error();
    };
    poll_state_locked(&network.sockets, handle)
}

/// 在 deferred source 通知中无等待地投影 raw IPv4 readiness。
///
/// # Parameters
///
/// - `handle`: raw smoltcp handle。
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
    let readiness = poll_state_locked(&network.sockets, handle);
    Some(readiness)
}

/// 从完整 protocol state 精确删除 raw endpoint 与 SocketSet slot。
///
/// # Parameters
///
/// - `network`: 已由 protocol owner 独占的完整 NetworkStack。
/// - `handle`: final InetSocket 唯一持有的 raw handle。
///
/// # Errors
///
/// 重复 identity 按幂等 remove 处理。
pub(super) fn drop_endpoint(network: &mut NetworkStack, handle: SocketHandle) {
    network.raw_endpoints.remove(&handle);
    network.sockets.remove(handle);
}
