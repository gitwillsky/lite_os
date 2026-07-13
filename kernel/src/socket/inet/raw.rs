use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::net::Ipv4Addr;

use smoltcp::{
    iface::SocketHandle,
    socket::raw,
    wire::{IpProtocol, IpVersion},
};

use super::{InetSocket, NetworkStack, stack};
use crate::{
    ipc::PipeEnd,
    socket::{InetAddress, SocketError, SocketPollState},
};

const IPV4_HEADER_LENGTH: usize = 20;
const RAW_PACKET_SLOTS: usize = 8;
const RAW_PACKET_CAPACITY: usize = 2048;
const DEFAULT_HOP_LIMIT: u8 = 64;

pub(super) struct RawEndpointState {
    pub(super) endpoint: Weak<InetSocket>,
    bound_address: Option<Ipv4Addr>,
    hop_limit: u8,
    broadcast: bool,
    device_bound: bool,
}

fn create_endpoint(
    network: &mut NetworkStack,
    endpoint: Weak<InetSocket>,
) -> Result<SocketHandle, SocketError> {
    let rx = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; RAW_PACKET_SLOTS],
        vec![0; RAW_PACKET_SLOTS * RAW_PACKET_CAPACITY],
    );
    let tx = raw::PacketBuffer::new(
        vec![raw::PacketMetadata::EMPTY; RAW_PACKET_SLOTS],
        vec![0; RAW_PACKET_SLOTS * RAW_PACKET_CAPACITY],
    );
    let handle = network.sockets.add(raw::Socket::new(
        Some(IpVersion::Ipv4),
        Some(IpProtocol::Icmp),
        rx,
        tx,
    ));
    network.raw_endpoints.insert(
        handle,
        RawEndpointState {
            endpoint,
            bound_address: None,
            hop_limit: DEFAULT_HOP_LIMIT,
            broadcast: false,
            device_bound: false,
        },
    );
    Ok(handle)
}

pub(super) fn new(notify: (Arc<PipeEnd>, Arc<PipeEnd>)) -> Result<Arc<InetSocket>, SocketError> {
    let mut network = stack()?.lock();
    let handle = create_endpoint(&mut network, Weak::new())?;
    let endpoint = Arc::new(InetSocket {
        endpoint: super::InetEndpoint::Raw(handle),
        notify_read: notify.0,
        notify_write: notify.1,
    });
    network
        .raw_endpoints
        .get_mut(&handle)
        .expect("new raw endpoint disappeared before Arc publication")
        .endpoint = Arc::downgrade(&endpoint);
    Ok(endpoint)
}

pub(super) type ReadinessSnapshot = Vec<(SocketHandle, Weak<InetSocket>, bool, bool)>;

pub(super) fn readiness_snapshot(network: &NetworkStack) -> ReadinessSnapshot {
    network
        .raw_endpoints
        .iter()
        .map(|(handle, state)| {
            let socket = network.sockets.get::<raw::Socket<'static>>(*handle);
            (
                *handle,
                state.endpoint.clone(),
                socket.can_recv(),
                socket.can_send(),
            )
        })
        .collect()
}

pub(super) fn readiness_notifications(
    network: &NetworkStack,
    before: ReadinessSnapshot,
) -> Vec<Arc<InetSocket>> {
    before
        .into_iter()
        .filter_map(|(handle, endpoint, was_readable, was_writable)| {
            let socket = network.sockets.get::<raw::Socket<'static>>(handle);
            (!was_readable && socket.can_recv() || !was_writable && socket.can_send())
                .then(|| endpoint.upgrade())
                .flatten()
        })
        .collect()
}

pub(super) fn bind(handle: SocketHandle, address: InetAddress) -> Result<(), SocketError> {
    if address.port != 0 {
        return Err(SocketError::Invalid);
    }
    let mut network = stack()?.lock();
    let configured = network.interface_state.address;
    if !address.address.is_unspecified() && Some(address.address) != configured {
        return Err(SocketError::AddressNotAvailable);
    }
    let state = network
        .raw_endpoints
        .get_mut(&handle)
        .ok_or(SocketError::NotConnected)?;
    state.bound_address = (!address.address.is_unspecified()).then_some(address.address);
    Ok(())
}

pub(super) fn address(handle: SocketHandle) -> Result<InetAddress, SocketError> {
    let network = stack()?.lock();
    let state = network
        .raw_endpoints
        .get(&handle)
        .ok_or(SocketError::NotConnected)?;
    Ok(InetAddress {
        address: state.bound_address.unwrap_or(Ipv4Addr::UNSPECIFIED),
        port: 0,
    })
}

pub(super) fn send(
    handle: SocketHandle,
    input: &[u8],
    target: Option<InetAddress>,
) -> Result<usize, SocketError> {
    let target = target.ok_or(SocketError::DestinationRequired)?;
    if target.port != 0 || input.len() + IPV4_HEADER_LENGTH > u16::MAX as usize {
        return Err(SocketError::MessageTooLarge);
    }
    let mut network = stack()?.lock();
    let state = network
        .raw_endpoints
        .get(&handle)
        .ok_or(SocketError::NotConnected)?;
    let source = state
        .bound_address
        .or(network.interface_state.address)
        .filter(|_| network.interface_state.up)
        .ok_or(SocketError::NetworkUnreachable)?;
    if target.address.is_broadcast() && !state.broadcast {
        return Err(SocketError::PermissionDenied);
    }
    if state.device_bound && !network.interface_state.up {
        return Err(SocketError::NetworkUnreachable);
    }
    let hop_limit = state.hop_limit;
    let total_length = IPV4_HEADER_LENGTH + input.len();
    let mut packet = vec![0; total_length];
    packet[0] = 0x45;
    packet[2..4].copy_from_slice(&(total_length as u16).to_be_bytes());
    packet[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
    packet[8] = hop_limit;
    packet[9] = 1;
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&target.address.octets());
    packet[IPV4_HEADER_LENGTH..].copy_from_slice(input);
    network
        .sockets
        .get_mut::<raw::Socket<'static>>(handle)
        .send_slice(&packet)
        .map_err(|_| SocketError::Again)?;
    Ok(input.len())
}

pub(super) fn receive(
    endpoint: &InetSocket,
    handle: SocketHandle,
    output: &mut [u8],
    peek: bool,
) -> Result<(usize, usize, InetAddress), SocketError> {
    let mut network = stack()?.lock();
    let socket = network.sockets.get_mut::<raw::Socket<'static>>(handle);
    let packet =
        if peek { socket.peek() } else { socket.recv() }.map_err(|_| SocketError::Again)?;
    if packet.len() < IPV4_HEADER_LENGTH {
        return Err(SocketError::Invalid);
    }
    let full_length = packet.len();
    let count = output.len().min(full_length);
    output[..count].copy_from_slice(&packet[..count]);
    let source = InetAddress {
        address: Ipv4Addr::from(<[u8; 4]>::try_from(&packet[12..16]).unwrap()),
        port: 0,
    };
    let drained = !peek && !socket.can_recv();
    drop(network);
    if drained {
        endpoint.consume_notify();
    }
    Ok((count, full_length, source))
}

pub(super) fn poll_state(handle: SocketHandle) -> SocketPollState {
    let Ok(network) = stack() else {
        return SocketPollState::error();
    };
    let network = network.lock();
    let socket = network.sockets.get::<raw::Socket<'static>>(handle);
    SocketPollState {
        readable: socket.can_recv(),
        writable: socket.can_send(),
        hangup: false,
        error: false,
    }
}

pub(super) fn set_broadcast(handle: SocketHandle, enabled: bool) -> Result<(), SocketError> {
    let mut network = stack()?.lock();
    network
        .raw_endpoints
        .get_mut(&handle)
        .ok_or(SocketError::NotConnected)?
        .broadcast = enabled;
    Ok(())
}

pub(super) fn bind_to_device(handle: SocketHandle, name: &[u8]) -> Result<(), SocketError> {
    let mut network = stack()?.lock();
    let state = network
        .raw_endpoints
        .get_mut(&handle)
        .ok_or(SocketError::NotConnected)?;
    match name {
        b"" => state.device_bound = false,
        b"eth0" => state.device_bound = true,
        _ => return Err(SocketError::NoDevice),
    }
    Ok(())
}

pub(super) fn set_hop_limit(handle: SocketHandle, value: u8) -> Result<(), SocketError> {
    let mut network = stack()?.lock();
    network
        .raw_endpoints
        .get_mut(&handle)
        .ok_or(SocketError::NotConnected)?
        .hop_limit = value;
    Ok(())
}

impl InetSocket {
    pub(in crate::socket) fn set_hop_limit(&self, value: u8) -> Result<(), SocketError> {
        if let super::InetEndpoint::Raw(handle) = self.endpoint {
            set_hop_limit(handle, value)
        } else {
            Err(SocketError::OperationNotSupported)
        }
    }
}

pub(super) fn drop_endpoint(handle: SocketHandle) {
    if let Some(network) = super::NETWORK_STACK.get() {
        let mut network = network.lock();
        network.raw_endpoints.remove(&handle);
        network.sockets.remove(handle);
    }
}
