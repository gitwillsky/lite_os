use super::{SocketType, current_task, errno, socket_error, socket_ofd};
use crate::net::{InternetOption, SocketIoDirection, SocketTimeout};

const IPPROTO_IP: usize = 0;
const IP_PKTINFO: usize = 8;
const IP_TTL: usize = 2;
const IP_MTU_DISCOVER: usize = 10;
const IP_MTU: usize = 14;
const IP_RECVERR: usize = 11;
const IP_ADD_MEMBERSHIP: usize = 35;
const IP_DROP_MEMBERSHIP: usize = 36;
const IP_MULTICAST_IF: usize = 32;
const IP_MULTICAST_TTL: usize = 33;
const IP_MULTICAST_LOOP: usize = 34;
const IP_MULTICAST_ALL: usize = 49;
const IPPROTO_TCP: usize = 6;
const IPPROTO_IPV6: usize = 41;
const IPPROTO_RAW: usize = 255;
const IPPROTO_ICMPV6: usize = 58;
const ICMP_FILTER: usize = 1;
const ICMPV6_FILTER: usize = 1;
const IPV6_CHECKSUM: usize = 7;
const IPV6_HDRINCL: usize = 36;
const IPV6_UNICAST_HOPS: usize = 16;
const IPV6_MULTICAST_IF: usize = 17;
const IPV6_MULTICAST_HOPS: usize = 18;
const IPV6_MULTICAST_LOOP: usize = 19;
const IPV6_JOIN_GROUP: usize = 20;
const IPV6_LEAVE_GROUP: usize = 21;
const IPV6_MULTICAST_ALL: usize = 29;
const IPV6_MTU: usize = 24;
const IPV6_DONTFRAG: usize = 62;
const IPV6_RECVERR: usize = 25;
const IPV6_V6ONLY: usize = 26;
const IPV6_RECVPKTINFO: usize = 49;
const TCP_NODELAY: usize = 1;
const TCP_MAXSEG: usize = 2;
const TCP_MD5SIG: usize = 14;
const TCP_MD5SIG_EXT: usize = 32;
const TCP_KEEPIDLE: usize = 4;
const TCP_KEEPINTVL: usize = 5;
const TCP_KEEPCNT: usize = 6;
const TCP_SYNCNT: usize = 7;
const TCP_USER_TIMEOUT: usize = 18;
const TCP_LINGER2: usize = 8;
const SOL_SOCKET: usize = 1;
const SOL_PACKET: usize = 263;
const PACKET_AUXDATA: usize = 8;
const SO_REUSEADDR: usize = 2;
const SO_TYPE: usize = 3;
const SO_ACCEPTCONN: usize = 30;
const SO_ERROR: usize = 4;
const SO_BROADCAST: usize = 6;
const SO_MARK: usize = 36;
const SO_SNDBUF: usize = 7;
const SO_RCVBUF: usize = 8;
const SO_KEEPALIVE: usize = 9;
const SO_SNDBUFFORCE: usize = 32;
const SO_RCVBUFFORCE: usize = 33;
const SO_PASSCRED: usize = 16;
const SO_PEERCRED: usize = 17;
const SO_BINDTODEVICE: usize = 25;
const SO_RCVTIMEO: usize = 20;
const SO_SNDTIMEO: usize = 21;
const SO_RCVTIMEO_NEW: usize = 66;
const SO_SNDTIMEO_NEW: usize = 67;
const IP_HDRINCL: usize = 3;
const IFNAMSIZ: usize = 16;

/// 设置已实现的 Linux IP 与 SOL_SOCKET endpoint policy。
///
/// # Parameters
///
/// - `fd`: socket descriptor。
/// - `level`: Linux option level。
/// - `option`: option number。
/// - `value`: option-specific userspace pointer。
/// - `length`: option buffer 长度。
///
/// # Returns
///
/// 成功返回零；descriptor、option、user-copy 或 domain 错误返回负 errno。
pub(crate) fn sys_setsockopt(
    fd: usize,
    level: usize,
    option: usize,
    value: usize,
    length: usize,
) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    // Linux optlen 是 32-bit signed int；负长度在选项裁剪与 user-copy 前拒绝。
    let length = length as u32 as i32;
    if length < 0 {
        return -errno::EINVAL;
    }
    let length = length as usize;
    if level == IPPROTO_IPV6 && socket.domain() != super::SocketDomain::Inet6 {
        return -errno::ENOPROTOOPT;
    }
    if level == IPPROTO_IP
        && !matches!(
            socket.domain(),
            super::SocketDomain::Inet | super::SocketDomain::Inet6
        )
    {
        return -errno::ENOPROTOOPT;
    }
    if level == SOL_PACKET && socket.domain() != super::SocketDomain::Packet {
        return -errno::ENOPROTOOPT;
    }
    if level == IPPROTO_TCP
        && (!matches!(
            socket.domain(),
            super::SocketDomain::Inet | super::SocketDomain::Inet6
        ) || socket.socket_type() != SocketType::Stream)
    {
        return -errno::ENOPROTOOPT;
    }
    match (level, option) {
        (IPPROTO_IP, IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP) => {
            if socket.socket_type() == SocketType::Stream {
                return -errno::EPROTO;
            }
            if length < 8 {
                return -errno::EINVAL;
            }
            let size = if length >= 12 { 12 } else { 8 };
            let mut request = [0u8; 12];
            if current_task()
                .unwrap()
                .copy_from_user(value, &mut request[..size])
                .is_err()
            {
                return -errno::EFAULT;
            }
            let group = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&request[..4]).unwrap());
            let address = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&request[4..8]).unwrap());
            let interface = u32::from_ne_bytes(request[8..12].try_into().unwrap());
            socket
                .ipv4_membership(group, interface, address, option == IP_ADD_MEMBERSHIP)
                .map_err(socket_error)
        }
        (IPPROTO_IP, IP_RECVERR) | (IPPROTO_IPV6, IPV6_RECVERR) => {
            let enabled = if level == IPPROTO_IP {
                if length == 0 {
                    Ok(0)
                } else {
                    read_ip_i32(value, length)
                }
            } else {
                read_i32(value, length)
            };
            enabled.and_then(|value| {
                socket
                    .set_receive_error(level == IPPROTO_IPV6, value != 0)
                    .map_err(socket_error)
            })
        }
        (IPPROTO_IP, IP_MTU_DISCOVER) => {
            let mode = if length == 0 {
                Ok(0)
            } else {
                read_ip_i32(value, length)
            };
            mode.and_then(|mode| {
                let mode = u8::try_from(mode)
                    .ok()
                    .filter(|mode| *mode <= 5)
                    .ok_or(-errno::EINVAL)?;
                socket.set_ipv4_mtu_discovery(mode).map_err(socket_error)
            })
        }
        (IPPROTO_IP, IP_MULTICAST_IF) => {
            if socket.socket_type() == SocketType::Stream || length < 4 {
                return -errno::EINVAL;
            }
            let size = if length >= 12 {
                12
            } else if length >= 8 {
                8
            } else {
                4
            };
            let mut request = [0; 12];
            if current_task()
                .unwrap()
                .copy_from_user(value, &mut request[..size])
                .is_err()
            {
                return -errno::EFAULT;
            }
            let offset = if size == 4 { 0 } else { 4 };
            let address = core::net::Ipv4Addr::from(
                <[u8; 4]>::try_from(&request[offset..offset + 4]).unwrap(),
            );
            let interface = u32::from_ne_bytes(request[8..12].try_into().unwrap());
            socket
                .set_ipv4_multicast_interface(interface, address)
                .map_err(socket_error)
        }
        (IPPROTO_IP, IP_MULTICAST_TTL | IP_MULTICAST_LOOP) => {
            if length == 0
                || option == IP_MULTICAST_TTL && socket.socket_type() == SocketType::Stream
            {
                return -errno::EINVAL;
            }
            read_ip_i32(value, length).and_then(|value| {
                let query = if option == IP_MULTICAST_TTL {
                    InternetOption::Ipv4MulticastHopLimit
                } else {
                    InternetOption::Ipv4MulticastLoop
                };
                socket
                    .set_ipv4_multicast_option(query, value)
                    .map_err(socket_error)
            })
        }
        (IPPROTO_IP, IP_MULTICAST_ALL) => {
            if length == 0 {
                return -errno::EINVAL;
            }
            read_ip_i32(value, length).and_then(|value| {
                if !matches!(value, 0 | 1) {
                    return Err(-errno::EINVAL);
                }
                socket
                    .set_ipv4_multicast_all(value != 0)
                    .map_err(socket_error)
            })
        }
        (IPPROTO_IPV6, IPV6_JOIN_GROUP | IPV6_LEAVE_GROUP) => {
            if socket.socket_type() == SocketType::Stream {
                return -errno::ENOPROTOOPT;
            }
            if length < 20 {
                return -errno::EINVAL;
            }
            let mut request = [0u8; 20];
            if current_task()
                .unwrap()
                .copy_from_user(value, &mut request)
                .is_err()
            {
                return -errno::EFAULT;
            }
            let group = core::net::Ipv6Addr::from(<[u8; 16]>::try_from(&request[..16]).unwrap());
            let interface = u32::from_ne_bytes(request[16..20].try_into().unwrap());
            socket
                .ipv6_membership(group, interface, option == IPV6_JOIN_GROUP)
                .map_err(socket_error)
        }
        (IPPROTO_RAW, ICMP_FILTER) | (IPPROTO_ICMPV6, ICMPV6_FILTER) => {
            set_icmp_filter(&socket, level, value, length)
        }
        (IPPROTO_IPV6 | IPPROTO_RAW, IPV6_CHECKSUM | IPV6_HDRINCL) => {
            if socket.domain() != super::SocketDomain::Inet6
                || socket.socket_type() != SocketType::Raw
            {
                return -errno::ENOPROTOOPT;
            }
            read_i32(value, length).and_then(|value| {
                if option == IPV6_HDRINCL {
                    return socket.set_header_included(value != 0).map_err(socket_error);
                }
                if level == IPPROTO_IPV6
                    && socket
                        .internet_option(InternetOption::Protocol)
                        .map_err(socket_error)?
                        == 58
                {
                    return Err(-errno::EINVAL);
                }
                socket
                    .set_ipv6_option(InternetOption::Ipv6Checksum, value)
                    .map_err(socket_error)
            })
        }
        (
            IPPROTO_IPV6,
            IPV6_V6ONLY | IPV6_UNICAST_HOPS | IPV6_RECVPKTINFO | IPV6_MTU | IPV6_MULTICAST_ALL
            | IPV6_MULTICAST_IF | IPV6_MULTICAST_HOPS | IPV6_MULTICAST_LOOP | IPV6_DONTFRAG,
        ) => read_i32(value, length).and_then(|value| {
            if matches!(option, IPV6_MULTICAST_IF | IPV6_MULTICAST_HOPS)
                && socket.socket_type() == SocketType::Stream
            {
                return Err(-errno::ENOPROTOOPT);
            }
            let query = match option {
                IPV6_V6ONLY => InternetOption::Ipv6Only,
                IPV6_RECVPKTINFO => InternetOption::Ipv6PacketInfo,
                IPV6_MTU => InternetOption::Ipv6Mtu,
                IPV6_DONTFRAG => InternetOption::Ipv6DontFragment,
                IPV6_MULTICAST_ALL => InternetOption::Ipv6MulticastAll,
                IPV6_MULTICAST_IF => InternetOption::Ipv6MulticastInterface,
                IPV6_MULTICAST_HOPS => InternetOption::Ipv6MulticastHopLimit,
                IPV6_MULTICAST_LOOP => InternetOption::Ipv6MulticastLoop,
                _ => InternetOption::Ipv6HopLimit,
            };
            if option == IPV6_UNICAST_HOPS && !(-1..=255).contains(&value) {
                return Err(-errno::EINVAL);
            }
            socket.set_ipv6_option(query, value).map_err(socket_error)
        }),
        (SOL_PACKET, PACKET_AUXDATA) => read_i32(value, length).and_then(|value| {
            socket
                .set_packet_auxiliary(value != 0)
                .map_err(socket_error)
        }),
        (IPPROTO_IP, IP_HDRINCL) => read_ip_i32(value, length)
            .and_then(|value| socket.set_header_included(value != 0).map_err(socket_error)),
        (IPPROTO_IP, IP_PKTINFO) => read_ip_i32(value, length).and_then(|value| {
            socket
                .set_ipv4_packet_info(value != 0)
                .map_err(socket_error)
        }),
        (IPPROTO_IP, IP_TTL) => {
            if length == 0 {
                return -errno::EINVAL;
            }
            read_ip_i32(value, length).and_then(|value| {
                let hop_limit = if value == -1 {
                    None
                } else {
                    Some(
                        u8::try_from(value)
                            .ok()
                            .filter(|value| *value != 0)
                            .ok_or(-errno::EINVAL)?,
                    )
                };
                socket.set_ipv4_hop_limit(hop_limit).map_err(socket_error)
            })
        }
        (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF | SO_SNDBUFFORCE | SO_RCVBUFFORCE) => {
            read_i32(value, length).and_then(|value| {
                let forced = matches!(option, SO_SNDBUFFORCE | SO_RCVBUFFORCE);
                if forced
                    && !current_task()
                        .unwrap()
                        .has_initial_capability(linux_abi::capability::Capability::NET_ADMIN)
                {
                    return Err(-errno::EPERM);
                }
                let direction = if matches!(option, SO_SNDBUF | SO_SNDBUFFORCE) {
                    SocketIoDirection::Send
                } else {
                    SocketIoDirection::Receive
                };
                socket
                    .set_buffer_limit(direction, value, forced)
                    .map_err(socket_error)
            })
        }
        (SOL_SOCKET, SO_PASSCRED) => read_enabled(value, length)
            .and_then(|enabled| socket.set_pass_credentials(enabled).map_err(socket_error)),
        (SOL_SOCKET, SO_REUSEADDR) => read_enabled(value, length)
            .and_then(|enabled| socket.set_reuse_address(enabled).map_err(socket_error)),
        (SOL_SOCKET, SO_BROADCAST) => read_enabled(value, length)
            .and_then(|enabled| socket.set_broadcast(enabled).map_err(socket_error)),
        (SOL_SOCKET, SO_MARK) => read_i32(value, length).and_then(|mark| {
            socket
                .set_mark(mark as u32, &current_task().unwrap().namespace_authority())
                .map_err(socket_error)
        }),
        (SOL_SOCKET, SO_BINDTODEVICE) => {
            read_interface_name(value, length).and_then(|(name, count)| {
                socket
                    .bind_to_device(
                        &name[..count],
                        &current_task().unwrap().namespace_authority(),
                    )
                    .map_err(socket_error)
            })
        }
        (SOL_SOCKET, SO_KEEPALIVE)
        | (
            IPPROTO_TCP,
            TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT | TCP_SYNCNT | TCP_USER_TIMEOUT
            | TCP_LINGER2,
        ) => read_i32(value, length).and_then(|value| {
            let query = match (level, option) {
                (SOL_SOCKET, SO_KEEPALIVE) => InternetOption::KeepAlive,
                (_, TCP_KEEPIDLE) => InternetOption::KeepIdle,
                (_, TCP_KEEPINTVL) => InternetOption::KeepInterval,
                (_, TCP_SYNCNT) => InternetOption::SynRetries,
                (_, TCP_USER_TIMEOUT) => InternetOption::UserTimeout,
                (_, TCP_LINGER2) => InternetOption::Linger2,
                _ => InternetOption::KeepCount,
            };
            socket
                .set_internet_timer(query, value)
                .map_err(socket_error)
        }),
        (IPPROTO_TCP, TCP_NODELAY) => read_enabled(value, length)
            .and_then(|enabled| socket.set_tcp_no_delay(enabled).map_err(socket_error)),
        (IPPROTO_TCP, TCP_MAXSEG) => read_i32(value, length)
            .and_then(|value| socket.set_tcp_max_segment(value).map_err(socket_error)),
        (IPPROTO_TCP, TCP_MD5SIG | TCP_MD5SIG_EXT) => {
            set_tcp_md5(&socket, value, length, option == TCP_MD5SIG_EXT)
        }
        (SOL_SOCKET, SO_RCVTIMEO | SO_RCVTIMEO_NEW) => {
            read_timeval(value, length).and_then(|timeout| {
                socket
                    .set_timeout(SocketIoDirection::Receive, timeout)
                    .map_err(socket_error)
            })
        }
        (SOL_SOCKET, SO_SNDTIMEO | SO_SNDTIMEO_NEW) => {
            read_timeval(value, length).and_then(|timeout| {
                socket
                    .set_timeout(SocketIoDirection::Send, timeout)
                    .map_err(socket_error)
            })
        }
        _ => Err(-errno::ENOPROTOOPT),
    }
    .map_or_else(|error| error, |()| 0)
}

fn read_enabled(value: usize, length: usize) -> Result<bool, isize> {
    read_i32(value, length).map(|value| value != 0)
}

fn set_tcp_md5(
    socket: &crate::net::Socket,
    value: usize,
    length: usize,
    extended: bool,
) -> Result<(), isize> {
    // 两个现有 64 位 ABI 的 sockaddr_storage 为 128 字节；只捕获一次完整 tcpm 命令，忽略尾部。
    if length < 216 {
        return Err(-errno::EINVAL);
    }
    let mut bytes = zeroize::Zeroizing::new([0; 216]);
    current_task()
        .unwrap()
        .copy_from_user(value, &mut *bytes)
        .map_err(|_| -errno::EFAULT)?;
    let family = u16::from_ne_bytes(bytes[..2].try_into().expect("fixed sockaddr family")) as usize;
    if family
        != if socket.domain() == super::SocketDomain::Inet {
            super::AF_INET
        } else {
            super::AF_INET6
        }
    {
        return Err(-errno::EINVAL);
    }
    let super::SocketAddress::Inet(address) = super::decode_address(&bytes[..128], false)? else {
        return Err(-errno::EINVAL);
    };
    // IFINDEX 在 Linux 指 L3 master，不得误当作物理 NIC；当前尚未提供 VRF/L3 master。
    if extended && bytes[128] & 2 != 0 {
        return Err(-errno::EOPNOTSUPP);
    }
    let address = address.ip().to_canonical();
    let mut selector = network_protocol::TcpMd5Selector::host(address, None);
    if extended && bytes[128] & 1 != 0 {
        selector.prefix_length = bytes[129];
    }
    let count = usize::from(u16::from_ne_bytes(
        bytes[130..132].try_into().expect("fixed key length"),
    ));
    if count > network_protocol::TCP_MD5_MAX_KEY_BYTES || !selector.valid() {
        return Err(-errno::EINVAL);
    }
    socket
        .set_tcp_md5(selector, &bytes[136..136 + count])
        .map_err(socket_error)
}

fn read_timeval(value: usize, length: usize) -> Result<SocketTimeout, isize> {
    if length < 16 {
        return Err(-errno::EINVAL);
    }
    let mut bytes = [0; 16];
    current_task()
        .unwrap()
        .copy_from_user(value, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    super::timeout::decode(bytes)
}

fn read_i32(value: usize, length: usize) -> Result<i32, isize> {
    if length < 4 {
        return Err(-errno::EINVAL);
    }
    let mut bytes = [0; 4];
    if value == 0
        || current_task()
            .unwrap()
            .copy_from_user(value, &mut bytes)
            .is_err()
    {
        return Err(-errno::EFAULT);
    }
    Ok(i32::from_ne_bytes(bytes))
}

/// native IP scalar option 接受 int 或单字节；零 optlen 对布尔选项等于零。
fn read_ip_i32(value: usize, length: usize) -> Result<i32, isize> {
    if length >= 4 {
        return read_i32(value, length);
    }
    if length == 0 {
        return Ok(0);
    }
    let mut byte = [0u8; 1];
    current_task()
        .unwrap()
        .copy_from_user(value, &mut byte)
        .map_err(|_| -errno::EFAULT)?;
    Ok(i32::from(byte[0]))
}

fn read_interface_name(value: usize, length: usize) -> Result<([u8; IFNAMSIZ], usize), isize> {
    if length == 0 {
        return Ok(([0; IFNAMSIZ], 0));
    }
    if value == 0 {
        return Err(-errno::EFAULT);
    }
    let mut bytes = [0; IFNAMSIZ];
    // 原生最多copy IFNAMSIZ-1，最后一个byte固定NUL；名字解析由实际NET接口owner完成。
    let count = length.min(IFNAMSIZ - 1);
    current_task()
        .unwrap()
        .copy_from_user(value, &mut bytes[..count])
        .map_err(|_| -errno::EFAULT)?;
    let name_length = bytes[..count]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(count);
    Ok((bytes, name_length))
}

/// 查询已实现的标准 socket/protocol options。
///
/// # Parameters
///
/// - `fd`: socket descriptor。
/// - `level`: Linux SOL_SOCKET/IPPROTO_IP/IPPROTO_TCP。
/// - `option`: 已接入实际 owner 的标准 option。
/// - `value`: output userspace pointer。
/// - `length`: 指向 input capacity/output actual length 的 userspace pointer。
///
/// # Returns
///
/// 成功返回零；descriptor、option 或 user-copy 错误返回负 errno。
pub(crate) fn sys_getsockopt(
    fd: usize,
    level: usize,
    option: usize,
    value: usize,
    length: usize,
) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if level == IPPROTO_IPV6 && socket.domain() != super::SocketDomain::Inet6 {
        return -errno::ENOPROTOOPT;
    }
    if level == IPPROTO_IP
        && !matches!(
            socket.domain(),
            super::SocketDomain::Inet | super::SocketDomain::Inet6
        )
    {
        return -errno::ENOPROTOOPT;
    }
    if level == SOL_PACKET && socket.domain() != super::SocketDomain::Packet {
        return -errno::ENOPROTOOPT;
    }
    if level == IPPROTO_TCP
        && (!matches!(
            socket.domain(),
            super::SocketDomain::Inet | super::SocketDomain::Inet6
        ) || socket.socket_type() != SocketType::Stream)
    {
        return -errno::ENOPROTOOPT;
    }
    if matches!(
        (level, option),
        (IPPROTO_RAW, ICMP_FILTER) | (IPPROTO_ICMPV6, ICMPV6_FILTER)
    ) {
        return get_icmp_filter(&socket, level, value, length).map_or_else(|error| error, |()| 0);
    }
    let task = current_task().unwrap();
    let mut size = [0u8; 4];
    if task.copy_from_user(length, &mut size).is_err() {
        return -errno::EFAULT;
    }
    let capacity = i32::from_ne_bytes(size);
    if capacity < 0 {
        return -errno::EINVAL;
    }
    if matches!(level, IPPROTO_IPV6 | IPPROTO_RAW)
        && matches!(option, IPV6_CHECKSUM | IPV6_HDRINCL)
        && (socket.domain() != super::SocketDomain::Inet6
            || socket.socket_type() != SocketType::Raw)
    {
        return -errno::ENOPROTOOPT;
    }
    let mut result = [0u8; 16];
    let result_length = match (level, option) {
        (SOL_PACKET, PACKET_AUXDATA) => {
            result[..4].copy_from_slice(&i32::from(socket.packet_auxiliary()).to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_TYPE) => {
            let value: i32 = match socket.socket_type() {
                SocketType::Stream => 1,
                SocketType::Datagram => 2,
                SocketType::Raw => 3,
                SocketType::SeqPacket => 5,
            };
            result[..4].copy_from_slice(&value.to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_ACCEPTCONN) => {
            let listening = match socket.is_listening() {
                Ok(value) => value,
                Err(error) => return socket_error(error),
            };
            result[..4].copy_from_slice(&i32::from(listening).to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_ERROR) => {
            let value = socket
                .take_error()
                .map_or(0, |error| (-socket_error(error)) as i32);
            result[..4].copy_from_slice(&value.to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF) => {
            let direction = if option == SO_SNDBUF {
                SocketIoDirection::Send
            } else {
                SocketIoDirection::Receive
            };
            let limit = match socket.buffer_limit(direction) {
                Ok(value) => value,
                Err(error) => return socket_error(error),
            };
            result[..4].copy_from_slice(&limit.to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_PASSCRED) => {
            result[..4].copy_from_slice(&i32::from(socket.pass_credentials()).to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_PEERCRED) => {
            result[..12].fill(0xff);
            if let Some(credentials) = socket.peer_credentials() {
                let pid = credentials.pid.number_in(&task.pid_namespace()) as i32;
                result[..4].copy_from_slice(&pid.to_ne_bytes());
                let namespace = task.user_namespace();
                result[4..8]
                    .copy_from_slice(&namespace.display_id(true, credentials.uid).to_ne_bytes());
                result[8..12]
                    .copy_from_slice(&namespace.display_id(false, credentials.gid).to_ne_bytes());
            }
            12
        }
        (SOL_SOCKET, SO_RCVTIMEO | SO_RCVTIMEO_NEW) => {
            result = super::timeout::encode(socket.timeout(SocketIoDirection::Receive));
            16
        }
        (SOL_SOCKET, SO_SNDTIMEO | SO_SNDTIMEO_NEW) => {
            result = super::timeout::encode(socket.timeout(SocketIoDirection::Send));
            16
        }
        (IPPROTO_IP, IP_MULTICAST_IF) => {
            let address = match socket.internet_option(InternetOption::Ipv4MulticastInterface) {
                Ok(address) => address as u32,
                Err(error) => return socket_error(error),
            };
            result[..4].copy_from_slice(&address.to_be_bytes());
            4
        }
        (SOL_SOCKET, SO_REUSEADDR | SO_BROADCAST | SO_KEEPALIVE | SO_MARK)
        | (
            IPPROTO_IP,
            IP_TTL | IP_MTU_DISCOVER | IP_MTU | IP_RECVERR | IP_PKTINFO | IP_HDRINCL
            | IP_MULTICAST_ALL | IP_MULTICAST_TTL | IP_MULTICAST_LOOP,
        )
        | (
            IPPROTO_IPV6,
            IPV6_V6ONLY | IPV6_UNICAST_HOPS | IPV6_RECVPKTINFO | IPV6_MTU | IPV6_MULTICAST_ALL
            | IPV6_MULTICAST_IF | IPV6_MULTICAST_HOPS | IPV6_MULTICAST_LOOP | IPV6_DONTFRAG
            | IPV6_RECVERR,
        )
        | (IPPROTO_IPV6 | IPPROTO_RAW, IPV6_CHECKSUM | IPV6_HDRINCL)
        | (
            IPPROTO_TCP,
            TCP_NODELAY | TCP_MAXSEG | TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT | TCP_SYNCNT
            | TCP_USER_TIMEOUT | TCP_LINGER2,
        ) => {
            let query = match (level, option) {
                (SOL_SOCKET, SO_REUSEADDR) => InternetOption::ReuseAddress,
                (SOL_SOCKET, SO_BROADCAST) => InternetOption::Broadcast,
                (SOL_SOCKET, SO_MARK) => InternetOption::Mark,
                (IPPROTO_IPV6, IPV6_V6ONLY) => InternetOption::Ipv6Only,
                (IPPROTO_IPV6, IPV6_RECVPKTINFO) => InternetOption::Ipv6PacketInfo,
                (IPPROTO_IPV6, IPV6_UNICAST_HOPS) => InternetOption::Ipv6HopLimit,
                (IPPROTO_IPV6, IPV6_MTU) => InternetOption::Ipv6Mtu,
                (IPPROTO_IPV6, IPV6_DONTFRAG) => InternetOption::Ipv6DontFragment,
                (IPPROTO_IPV6, IPV6_MULTICAST_ALL) => InternetOption::Ipv6MulticastAll,
                (IPPROTO_IPV6, IPV6_MULTICAST_IF) => InternetOption::Ipv6MulticastInterface,
                (IPPROTO_IPV6, IPV6_MULTICAST_HOPS) => InternetOption::Ipv6MulticastHopLimit,
                (IPPROTO_IPV6, IPV6_MULTICAST_LOOP) => InternetOption::Ipv6MulticastLoop,
                (IPPROTO_IPV6 | IPPROTO_RAW, IPV6_CHECKSUM) => InternetOption::Ipv6Checksum,
                (IPPROTO_IPV6 | IPPROTO_RAW, IPV6_HDRINCL) => InternetOption::HeaderIncluded,
                (IPPROTO_IP, IP_MTU_DISCOVER) => InternetOption::Ipv4MtuDiscovery,
                (IPPROTO_IP, IP_MTU) => InternetOption::Ipv4Mtu,
                (IPPROTO_IP, IP_RECVERR) => InternetOption::Ipv4ReceiveError,
                (IPPROTO_IPV6, IPV6_RECVERR) => InternetOption::Ipv6ReceiveError,
                (IPPROTO_IP, IP_TTL) => InternetOption::HopLimit,
                (IPPROTO_IP, IP_HDRINCL) => InternetOption::HeaderIncluded,
                (IPPROTO_IP, IP_PKTINFO) => InternetOption::PacketInfo,
                (IPPROTO_IP, IP_MULTICAST_ALL) => InternetOption::Ipv4MulticastAll,
                (IPPROTO_IP, IP_MULTICAST_TTL) => InternetOption::Ipv4MulticastHopLimit,
                (IPPROTO_IP, IP_MULTICAST_LOOP) => InternetOption::Ipv4MulticastLoop,
                (IPPROTO_TCP, TCP_MAXSEG) => InternetOption::MaxSegment,
                (SOL_SOCKET, SO_KEEPALIVE) => InternetOption::KeepAlive,
                (IPPROTO_TCP, TCP_KEEPIDLE) => InternetOption::KeepIdle,
                (IPPROTO_TCP, TCP_KEEPINTVL) => InternetOption::KeepInterval,
                (IPPROTO_TCP, TCP_KEEPCNT) => InternetOption::KeepCount,
                (IPPROTO_TCP, TCP_SYNCNT) => InternetOption::SynRetries,
                (IPPROTO_TCP, TCP_USER_TIMEOUT) => InternetOption::UserTimeout,
                (IPPROTO_TCP, TCP_LINGER2) => InternetOption::Linger2,
                _ => InternetOption::NoDelay,
            };
            let actual = match socket.internet_option(query) {
                Ok(value) => value,
                Err(error) => return socket_error(error),
            };
            result[..4].copy_from_slice(&actual.to_ne_bytes());
            4
        }
        (SOL_SOCKET, SO_BINDTODEVICE) => {
            let bound = match socket.internet_option(InternetOption::BoundToDevice) {
                Ok(value) => value as u32,
                Err(error) => return socket_error(error),
            };
            if bound != 0 {
                if capacity < IFNAMSIZ as i32 {
                    return -errno::EINVAL;
                }
                let snapshot = match super::interface_snapshot(&socket.net_namespace(), bound) {
                    Ok(snapshot) => snapshot,
                    Err(error) => return socket_error(error),
                };
                let count = snapshot
                    .name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(IFNAMSIZ - 1)
                    + 1;
                result[..count].copy_from_slice(&snapshot.name[..count]);
                count
            } else {
                0
            }
        }
        _ => return -errno::ENOPROTOOPT,
    };
    let count = if level == IPPROTO_IP && option != IP_MULTICAST_IF && capacity > 0 && capacity < 4
    {
        1
    } else {
        (capacity as usize).min(result_length)
    };
    // IP/TCP scalar getter 原生先写 optlen；SOL_SOCKET 原生先写 value 后写 optlen。
    if level != SOL_SOCKET {
        if task
            .copy_to_user(length, &(count as u32).to_ne_bytes())
            .is_err()
            || (count != 0 && task.copy_to_user(value, &result[..count]).is_err())
        {
            return -errno::EFAULT;
        }
    } else if (count != 0 && task.copy_to_user(value, &result[..count]).is_err())
        || task
            .copy_to_user(length, &(count as u32).to_ne_bytes())
            .is_err()
    {
        return -errno::EFAULT;
    }
    0
}

/// 先核对不可变协议身份，再访问用户缓冲，保持原生 RAW 的错误优先级。
fn icmp_filter_size(socket: &crate::net::Socket, level: usize) -> Result<usize, isize> {
    let (domain, protocol, size) = if level == IPPROTO_ICMPV6 {
        (super::SocketDomain::Inet6, 58, 32)
    } else {
        (super::SocketDomain::Inet, 1, 4)
    };
    if socket.domain() != domain || socket.socket_type() != SocketType::Raw {
        return Err(-errno::ENOPROTOOPT);
    }
    if socket
        .internet_option(InternetOption::Protocol)
        .map_err(socket_error)?
        != protocol
    {
        return Err(-errno::EOPNOTSUPP);
    }
    Ok(size)
}
fn set_icmp_filter(
    socket: &crate::net::Socket,
    level: usize,
    value: usize,
    length: usize,
) -> Result<(), isize> {
    let length = length.min(icmp_filter_size(socket, level)?);
    let mut bytes = [0; 32];
    if length != 0 {
        current_task()
            .unwrap()
            .copy_from_user(value, &mut bytes[..length])
            .map_err(|_| -errno::EFAULT)?;
    }
    let mut update = network_protocol::IcmpFilterUpdate::default();
    for (index, (mask, blocked)) in update
        .mask
        .iter_mut()
        .zip(update.blocked.iter_mut())
        .enumerate()
    {
        let mut mask_bytes = [0; 4];
        for (offset, byte) in mask_bytes.iter_mut().enumerate() {
            if index * 4 + offset < length {
                *byte = 255;
            }
        }
        *mask = u32::from_ne_bytes(mask_bytes);
        *blocked = u32::from_ne_bytes(
            bytes[index * 4..index * 4 + 4]
                .try_into()
                .expect("fixed filter word"),
        );
    }
    socket
        .icmp_filter(Some(update))
        .map(|_| ())
        .map_err(socket_error)
}
fn get_icmp_filter(
    socket: &crate::net::Socket,
    level: usize,
    value: usize,
    length: usize,
) -> Result<(), isize> {
    let maximum = icmp_filter_size(socket, level)?;
    let task = current_task().unwrap();
    let mut capacity = [0; 4];
    task.copy_from_user(length, &mut capacity)
        .map_err(|_| -errno::EFAULT)?;
    let capacity = i32::from_ne_bytes(capacity);
    if capacity < 0 {
        return Err(-errno::EINVAL);
    }
    let count = (capacity as usize).min(maximum);
    let filter = socket.icmp_filter(None).map_err(socket_error)?;
    let mut bytes = [0; 32];
    for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(filter) {
        *chunk = word.to_ne_bytes();
    }
    // 与标准 RAW getter 一致：先写实际 optlen，零长度不访问 optval。
    task.copy_to_user(length, &(count as i32).to_ne_bytes())
        .map_err(|_| -errno::EFAULT)?;
    if count != 0 {
        task.copy_to_user(value, &bytes[..count])
            .map_err(|_| -errno::EFAULT)?;
    }
    Ok(())
}
