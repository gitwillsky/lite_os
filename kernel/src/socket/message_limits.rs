#[cfg(not(test))]
use super::{SocketDomain, SocketType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MessageProtocol {
    Stream,
    UnixDatagram,
    Ipv4Udp,
    Ipv4Raw,
    Ipv4Packet,
    NetlinkUevent,
    Unsupported,
}

pub(super) const MAX_UNIX_DATAGRAM_BYTES: usize = 65_535;
pub(super) const MAX_IPV4_UDP_BYTES: usize = 65_507;
pub(super) const MAX_IPV4_RAW_BYTES: usize = u16::MAX as usize - 20;
pub(super) const MAX_IPV4_PACKET_BYTES: usize = 1_500;

#[cfg(not(test))]
pub(super) fn protocol(domain: SocketDomain, socket_type: SocketType) -> MessageProtocol {
    match (domain, socket_type) {
        (_, SocketType::Stream) => MessageProtocol::Stream,
        (SocketDomain::Unix, SocketType::Datagram) => MessageProtocol::UnixDatagram,
        (SocketDomain::Inet, SocketType::Datagram) => MessageProtocol::Ipv4Udp,
        (SocketDomain::Inet, SocketType::Raw) => MessageProtocol::Ipv4Raw,
        (SocketDomain::Packet, SocketType::Datagram) => MessageProtocol::Ipv4Packet,
        (SocketDomain::Netlink, SocketType::Datagram) => MessageProtocol::NetlinkUevent,
        _ => MessageProtocol::Unsupported,
    }
}

fn maximum_send_length(protocol: MessageProtocol) -> Option<usize> {
    match protocol {
        MessageProtocol::Stream => None,
        MessageProtocol::UnixDatagram => Some(MAX_UNIX_DATAGRAM_BYTES),
        MessageProtocol::Ipv4Udp => Some(MAX_IPV4_UDP_BYTES),
        MessageProtocol::Ipv4Raw => Some(MAX_IPV4_RAW_BYTES),
        MessageProtocol::Ipv4Packet => Some(MAX_IPV4_PACKET_BYTES),
        MessageProtocol::NetlinkUevent => Some(u16::MAX as usize),
        MessageProtocol::Unsupported => Some(0),
    }
}

/// @description 在 payload gather 前验证 protocol-owned atomic message bound。
pub(super) fn validate_send_length(protocol: MessageProtocol, length: usize) -> Result<(), ()> {
    if maximum_send_length(protocol).is_some_and(|maximum| length > maximum) {
        Err(())
    } else {
        Ok(())
    }
}

/// @description 为 stream send 选择一次 backend call 的固定上限 staging capacity。
/// @return stream 返回有界 capacity；atomic/unsupported protocol 返回 None。
pub(super) fn stream_send_capacity(
    protocol: MessageProtocol,
    requested: usize,
    stream_maximum: usize,
) -> Option<usize> {
    match protocol {
        MessageProtocol::Stream => Some(requested.min(stream_maximum)),
        _ => None,
    }
}

/// @description 把 userspace receive capacity 投影为一次 backend call 的最大有用 storage。
pub(super) fn receive_capacity(
    protocol: MessageProtocol,
    requested: usize,
    stream_maximum: usize,
) -> usize {
    let maximum = match protocol {
        MessageProtocol::Stream => stream_maximum,
        MessageProtocol::UnixDatagram => MAX_UNIX_DATAGRAM_BYTES,
        MessageProtocol::Ipv4Udp => MAX_IPV4_UDP_BYTES,
        // Raw receive 含内核重建的 IPv4 header，最大为完整 u16 total length。
        MessageProtocol::Ipv4Raw => u16::MAX as usize,
        MessageProtocol::Ipv4Packet => MAX_IPV4_PACKET_BYTES,
        MessageProtocol::NetlinkUevent => u16::MAX as usize,
        MessageProtocol::Unsupported => 0,
    };
    requested.min(maximum)
}
