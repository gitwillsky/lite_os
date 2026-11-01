#[cfg(not(test))]
use super::{SocketDomain, SocketType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MessageProtocol {
    Stream,
    UnixDatagram,
    Ipv4Udp,
    Ipv4Raw,
    Packet,
    NetlinkUevent,
    Unsupported,
}

pub(super) const MAX_IPV4_UDP_BYTES: usize = 65_507;
// HDRINCL 输入包含完整 IPv4 header；具体 codec/MTU 上限由实际 RAW owner 校验。
pub(super) const MAX_IPV4_RAW_BYTES: usize = u16::MAX as usize;
pub(super) const MAX_PACKET_BYTES: usize = 65_536 + 14;

#[cfg(not(test))]
pub(super) fn protocol(domain: SocketDomain, socket_type: SocketType) -> MessageProtocol {
    match (domain, socket_type) {
        (_, SocketType::Stream) => MessageProtocol::Stream,
        (SocketDomain::Unix, SocketType::Datagram | SocketType::SeqPacket) => {
            MessageProtocol::UnixDatagram
        }
        (SocketDomain::Inet, SocketType::Datagram) => MessageProtocol::Ipv4Udp,
        (SocketDomain::Inet, SocketType::Raw) => MessageProtocol::Ipv4Raw,
        (SocketDomain::Packet, SocketType::Datagram | SocketType::Raw) => MessageProtocol::Packet,
        (SocketDomain::Netlink, SocketType::Datagram | SocketType::Raw) => {
            MessageProtocol::NetlinkUevent
        }
        _ => MessageProtocol::Unsupported,
    }
}

fn maximum_send_length(protocol: MessageProtocol) -> Option<usize> {
    match protocol {
        MessageProtocol::Stream => None,
        // AF_UNIX 实际 sndbuf 校验由 endpoint owner 完成，无第二份固定 payload 上限。
        MessageProtocol::UnixDatagram => None,
        MessageProtocol::Ipv4Udp => Some(MAX_IPV4_UDP_BYTES),
        MessageProtocol::Ipv4Raw => Some(MAX_IPV4_RAW_BYTES),
        MessageProtocol::Packet => Some(MAX_PACKET_BYTES),
        MessageProtocol::NetlinkUevent => Some(u16::MAX as usize),
        MessageProtocol::Unsupported => Some(0),
    }
}

/// 在 payload gather 前验证 protocol-owned atomic message bound。
pub(super) fn validate_send_length(protocol: MessageProtocol, length: usize) -> Result<(), ()> {
    if maximum_send_length(protocol).is_some_and(|maximum| length > maximum) {
        Err(())
    } else {
        Ok(())
    }
}

/// 为 stream send 选择一次 backend call 的固定上限 staging capacity。
///
/// # Returns
///
/// stream 返回有界 capacity；atomic/unsupported protocol 返回 None。
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

/// 把 userspace receive capacity 投影为一次 backend call 的最大有用 storage。
pub(super) fn receive_capacity(
    protocol: MessageProtocol,
    requested: usize,
    stream_maximum: usize,
) -> usize {
    let maximum = match protocol {
        MessageProtocol::Stream => stream_maximum,
        MessageProtocol::UnixDatagram => requested,
        MessageProtocol::Ipv4Udp => MAX_IPV4_UDP_BYTES,
        // Raw receive 含内核重建的 IPv4 header，最大为完整 u16 total length。
        MessageProtocol::Ipv4Raw => u16::MAX as usize,
        MessageProtocol::Packet => MAX_PACKET_BYTES,
        MessageProtocol::NetlinkUevent => u16::MAX as usize,
        MessageProtocol::Unsupported => 0,
    };
    requested.min(maximum)
}
