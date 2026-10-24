use super::{SocketType, current_task, errno, socket_error, socket_ofd};
use crate::socket::{InternetOption, SocketIoDirection, SocketTimeout};

const IPPROTO_IP: usize = 0;
const IP_PKTINFO: usize = 8;
const IP_TTL: usize = 2;
const IPPROTO_TCP: usize = 6;
const TCP_NODELAY: usize = 1;
const TCP_MAXSEG: usize = 2;
const TCP_KEEPIDLE: usize = 4;
const TCP_KEEPINTVL: usize = 5;
const TCP_KEEPCNT: usize = 6;
const TCP_SYNCNT: usize = 7;
const TCP_USER_TIMEOUT: usize = 18;
const TCP_LINGER2: usize = 8;
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_TYPE: usize = 3;
const SO_ACCEPTCONN: usize = 30;
const SO_ERROR: usize = 4;
const SO_BROADCAST: usize = 6;
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
    let length = length as u32 as i32;
    if length < 0 {
        return -errno::EINVAL;
    }
    let length = length as usize;
    if level == IPPROTO_TCP
        && (socket.domain() != super::SocketDomain::Inet
            || socket.socket_type() != SocketType::Stream)
    {
        return -errno::ENOPROTOOPT;
    }
    match (level, option) {
        (IPPROTO_IP, IP_HDRINCL) => read_ip_i32(value, length).and_then(|value| {
            socket
                .set_ipv4_header_included(value != 0)
                .map_err(socket_error)
        }),
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
                        .has_initial_capability(syscall_abi::capability::Capability::NET_ADMIN)
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
    if level == IPPROTO_TCP
        && (socket.domain() != super::SocketDomain::Inet
            || socket.socket_type() != SocketType::Stream)
    {
        return -errno::ENOPROTOOPT;
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
    let mut result = [0u8; 16];
    let result_length = match (level, option) {
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
        (SOL_SOCKET, SO_REUSEADDR | SO_BROADCAST | SO_KEEPALIVE)
        | (IPPROTO_IP, IP_TTL | IP_PKTINFO | IP_HDRINCL)
        | (
            IPPROTO_TCP,
            TCP_NODELAY | TCP_MAXSEG | TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT | TCP_SYNCNT
            | TCP_USER_TIMEOUT | TCP_LINGER2,
        ) => {
            let query = match (level, option) {
                (SOL_SOCKET, SO_REUSEADDR) => InternetOption::ReuseAddress,
                (SOL_SOCKET, SO_BROADCAST) => InternetOption::Broadcast,
                (IPPROTO_IP, IP_TTL) => InternetOption::HopLimit,
                (IPPROTO_IP, IP_HDRINCL) => InternetOption::HeaderIncluded,
                (IPPROTO_IP, IP_PKTINFO) => InternetOption::PacketInfo,
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
    let count = if level == IPPROTO_IP && capacity > 0 && capacity < 4 {
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
