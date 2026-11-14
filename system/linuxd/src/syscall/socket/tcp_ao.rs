//! Linux v6.12 TCP-AO socket UAPI. Native TCP owns keys, policy and connection state.
use super::{
    AF_INET, AF_INET6, SocketAddress, SocketDomain, current_task, decode_address, errno,
    socket_error,
};
use crate::net::{Socket, SocketError};
use core::net::IpAddr;
use network_protocol::{
    TCP_AO_MAX_KEY_BYTES, TcpAoAlgorithm, TcpAoKey, TcpAoSelection, TcpKeySelector,
};
use zeroize::Zeroizing;

pub(super) const TCP_AO_ADD_KEY: usize = 38;
pub(super) const TCP_AO_DEL_KEY: usize = 39;
pub(super) const TCP_AO_INFO: usize = 40;

const ADD_BYTES: usize = 288;
const DEL_BYTES: usize = 144;
const INFO_BYTES: usize = 48;

/// Capture exactly the supported structure once. Linux permits a larger zeroed tail.
fn capture<const N: usize>(value: usize, length: usize) -> Result<Zeroizing<[u8; N]>, isize> {
    if length < N {
        return Err(-errno::EINVAL);
    }
    let mut bytes = Zeroizing::new([0; N]);
    copy_struct(value, length, &mut *bytes)?;
    Ok(bytes)
}

fn copy_struct(value: usize, length: usize, bytes: &mut [u8]) -> Result<(), isize> {
    value.checked_add(length).ok_or(-errno::EFAULT)?;
    let task = current_task().unwrap();
    let size = length.min(bytes.len());
    task.copy_from_user(value, &mut bytes[..size])
        .map_err(|_| -errno::EFAULT)?;
    let mut tail = Zeroizing::new([0; 128]);
    for offset in (size..length).step_by(tail.len()) {
        let chunk = (length - offset).min(tail.len());
        let tail = &mut tail[..chunk];
        task.copy_from_user(value + offset, tail)
            .map_err(|_| -errno::EFAULT)?;
        if tail.iter().any(|&byte| byte != 0) {
            return Err(-errno::E2BIG);
        }
    }
    Ok(())
}

fn selector(
    domain: SocketDomain,
    sockaddr: &[u8],
    prefix: u8,
    ifindex: i32,
    flags: u8,
) -> Result<TcpKeySelector, isize> {
    // Linux's ifindex is a VRF master, not a physical interface.
    if flags & 1 != 0 {
        return Err(-errno::EOPNOTSUPP);
    }
    if ifindex != 0 {
        return Err(-errno::EINVAL);
    }
    let family = u16::from_ne_bytes(sockaddr[..2].try_into().expect("sockaddr family")) as usize;
    let expected = if domain == SocketDomain::Inet {
        AF_INET
    } else {
        AF_INET6
    };
    if family != expected {
        return Err(-errno::EINVAL);
    }
    let SocketAddress::Inet(address) = decode_address(sockaddr, false)? else {
        return Err(-errno::EINVAL);
    };
    let ip = address.ip().to_canonical();
    let key = TcpKeySelector {
        address: ip,
        prefix_length: prefix,
        interface_index: None,
    };
    if address.port() != 0 || !key.valid() || (prefix == 0) != ip.is_unspecified() {
        return Err(-errno::EINVAL);
    }
    let host = match ip {
        IpAddr::V4(ip) => u32::from(ip) & u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0) != 0,
        IpAddr::V6(ip) => {
            u128::from(ip) & u128::MAX.checked_shr(u32::from(prefix)).unwrap_or(0) != 0
        }
    };
    if host {
        return Err(-errno::EINVAL);
    }
    Ok(key)
}

fn selection(flags: u32, current: u8, receive: u8) -> TcpAoSelection {
    TcpAoSelection {
        current: (flags & 1 != 0).then_some(current),
        receive: (flags & 2 != 0).then_some(receive),
    }
}

fn add(socket: &Socket, bytes: &[u8; ADD_BYTES]) -> Result<(), isize> {
    let flags = u32::from_ne_bytes(bytes[196..200].try_into().expect("AO add flags"));
    let key_flags = bytes[206];
    let length = usize::from(bytes[207]);
    if flags & !3 != 0
        || bytes[200..202] != [0; 2]
        || key_flags & !3 != 0
        || length == 0
        || length > TCP_AO_MAX_KEY_BYTES
    {
        return Err(-errno::EINVAL);
    }
    if !matches!(bytes[205], 0 | 12) {
        return Err(-errno::EOPNOTSUPP);
    }
    let name = &bytes[128..192];
    let name = &name[..name.iter().position(|&byte| byte == 0).unwrap_or(63)];
    let algorithm = match name {
        b"hmac(sha1)" => TcpAoAlgorithm::HmacSha1,
        b"cmac(aes128)" => TcpAoAlgorithm::Aes128Cmac,
        b"cmac(aes)" if length == 16 => TcpAoAlgorithm::Aes128Cmac,
        _ => return Err(-errno::EOPNOTSUPP),
    };
    let config = TcpAoKey {
        selector: selector(
            socket.domain(),
            &bytes[..128],
            bytes[202],
            i32::from_ne_bytes(bytes[192..196].try_into().expect("AO ifindex")),
            key_flags,
        )?,
        algorithm,
        send_id: bytes[203],
        receive_id: bytes[204],
        cover_options: key_flags & 2 == 0,
    };
    socket
        .set_tcp_ao_key(
            config,
            &bytes[208..208 + length],
            selection(flags, config.send_id, config.receive_id),
        )
        .map_err(|error| {
            if error == SocketError::AddressInUse {
                -errno::EEXIST
            } else {
                socket_error(error)
            }
        })
}

fn delete(socket: &Socket, bytes: &[u8; DEL_BYTES]) -> Result<(), isize> {
    let flags = u32::from_ne_bytes(bytes[132..136].try_into().expect("AO delete flags"));
    let key_flags = bytes[143];
    if flags & !7 != 0 || bytes[136..138] != [0; 2] || key_flags & !1 != 0 {
        return Err(-errno::EINVAL);
    }
    if flags & 4 != 0 {
        return Err(-errno::EOPNOTSUPP);
    }
    let config = TcpAoKey {
        selector: selector(
            socket.domain(),
            &bytes[..128],
            bytes[138],
            i32::from_ne_bytes(bytes[128..132].try_into().expect("AO ifindex")),
            key_flags,
        )?,
        algorithm: TcpAoAlgorithm::HmacSha1,
        send_id: bytes[139],
        receive_id: bytes[140],
        cover_options: true,
    };
    socket
        .set_tcp_ao_key(config, &[], selection(flags, bytes[141], bytes[142]))
        .map_err(socket_error)
}

fn info_flags(bytes: &[u8; INFO_BYTES]) -> Result<u32, isize> {
    let flags = u32::from_ne_bytes(bytes[..4].try_into().expect("AO info flags"));
    if flags & !31 != 0 || bytes[4..6] != [0; 2] {
        return Err(-errno::EINVAL);
    }
    Ok(flags)
}

fn set_info(socket: &Socket, bytes: &[u8; INFO_BYTES]) -> Result<(), isize> {
    let flags = info_flags(bytes)?;
    if flags & 28 != 0 {
        return Err(-errno::EOPNOTSUPP);
    }
    socket
        .select_tcp_ao_keys(selection(flags, bytes[6], bytes[7]))
        .map_err(socket_error)
}

/// Linux preserves optlen and returns only the prefix the caller requested.
pub(super) fn get_info(socket: &Socket, value: usize, length: usize) -> Result<(), isize> {
    let task = current_task().unwrap();
    let mut len = [0; 4];
    task.copy_from_user(length, &mut len)
        .map_err(|_| -errno::EFAULT)?;
    let len = i32::from_ne_bytes(len);
    if len <= 0 {
        return Err(-errno::EINVAL);
    }
    let len = len as usize;
    let mut input = [0; INFO_BYTES];
    copy_struct(value, len, &mut input)?;
    info_flags(&input)?;
    let status = socket.tcp_ao_status().map_err(socket_error)?;
    let mut output = [0; INFO_BYTES];
    let flags = u32::from(status.selection.current.is_some())
        | (u32::from(status.selection.receive.is_some()) << 1);
    output[..4].copy_from_slice(&flags.to_ne_bytes());
    output[6] = status.selection.current.unwrap_or(0);
    output[7] = status.selection.receive.unwrap_or(0);
    let counters = status.counters;
    for (field, count) in output[8..].as_chunks_mut::<8>().0.iter_mut().zip([
        counters.verified,
        counters.bad_mac,
        counters.key_not_found,
        counters.unsigned,
        counters.ignored_icmp,
    ]) {
        field.copy_from_slice(&count.to_ne_bytes());
    }
    task.copy_to_user(value, &output[..len.min(INFO_BYTES)])
        .map_err(|_| -errno::EFAULT)
}

pub(super) fn set_option(
    socket: &Socket,
    option: usize,
    value: usize,
    length: usize,
) -> Result<(), isize> {
    match option {
        TCP_AO_ADD_KEY => add(socket, &*capture(value, length)?),
        TCP_AO_DEL_KEY => delete(socket, &*capture(value, length)?),
        TCP_AO_INFO => set_info(socket, &*capture(value, length)?),
        _ => Err(-errno::ENOPROTOOPT),
    }
}
