use super::{SocketType, current_task, errno, socket_error, socket_ofd};

const IPPROTO_IP: usize = 0;
const IP_PKTINFO: usize = 8;
const IP_TTL: usize = 2;
const IPPROTO_TCP: usize = 6;
const TCP_NODELAY: usize = 1;
const SOL_SOCKET: usize = 1;
const SO_REUSEADDR: usize = 2;
const SO_TYPE: usize = 3;
const SO_ERROR: usize = 4;
const SO_BROADCAST: usize = 6;
const SO_PEERCRED: usize = 17;
const SO_BINDTODEVICE: usize = 25;
const IFNAMSIZ: usize = 16;

/// @description 设置已实现的 Linux IP 与 SOL_SOCKET endpoint policy。
///
/// @param fd socket descriptor。
/// @param level Linux option level。
/// @param option option number。
/// @param value option-specific userspace pointer。
/// @param length option buffer 长度。
/// @return 成功返回零；descriptor、option、user-copy 或 domain 错误返回负 errno。
pub(crate) fn sys_setsockopt(
    fd: usize,
    level: usize,
    option: usize,
    value: usize,
    length: usize,
) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match (level, option) {
        (IPPROTO_IP, IP_PKTINFO) => read_enabled(value, length)
            .and_then(|enabled| socket.set_ipv4_packet_info(enabled).map_err(socket_error)),
        (IPPROTO_IP, IP_TTL) => read_i32(value, length).and_then(|value| {
            u8::try_from(value)
                .ok()
                .filter(|value| *value != 0)
                .ok_or(-errno::EINVAL)
                .and_then(|value| socket.set_ipv4_hop_limit(value).map_err(socket_error))
        }),
        (SOL_SOCKET, SO_REUSEADDR) => read_enabled(value, length)
            .and_then(|enabled| socket.set_reuse_address(enabled).map_err(socket_error)),
        (SOL_SOCKET, SO_BROADCAST) => read_enabled(value, length)
            .and_then(|enabled| socket.set_broadcast(enabled).map_err(socket_error)),
        (SOL_SOCKET, SO_BINDTODEVICE) => read_interface_name(value, length)
            .and_then(|name| socket.bind_to_device(name).map_err(socket_error)),
        (IPPROTO_TCP, TCP_NODELAY) => read_enabled(value, length)
            .and_then(|enabled| socket.set_tcp_no_delay(enabled).map_err(socket_error)),
        _ => Err(-errno::ENOPROTOOPT),
    }
    .map_or_else(|error| error, |()| 0)
}

fn read_enabled(value: usize, length: usize) -> Result<bool, isize> {
    read_i32(value, length).map(|value| value != 0)
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

fn read_interface_name(value: usize, length: usize) -> Result<&'static [u8], isize> {
    if length == 0 {
        return Ok(&[]);
    }
    if value == 0 {
        return Err(-errno::EFAULT);
    }
    let mut bytes = [0; IFNAMSIZ];
    let count = length.min(IFNAMSIZ);
    current_task()
        .unwrap()
        .copy_from_user(value, &mut bytes[..count])
        .map_err(|_| -errno::EFAULT)?;
    let name_length = bytes[..count]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(count);
    match &bytes[..name_length] {
        b"" => Ok(&[]),
        b"eth0" => Ok(b"eth0"),
        _ => Err(-errno::ENODEV),
    }
}

/// @description 查询 Linux SOL_SOCKET 的只读 socket type 与 pending error。
///
/// @param fd socket descriptor。
/// @param level Linux option level，必须为 `SOL_SOCKET`。
/// @param option `SO_TYPE` 或 `SO_ERROR`。
/// @param value output userspace pointer。
/// @param length 指向 input capacity/output actual length 的 userspace pointer。
/// @return 成功返回零；descriptor、option 或 user-copy 错误返回负 errno。
pub(crate) fn sys_getsockopt(
    fd: usize,
    level: usize,
    option: usize,
    value: usize,
    length: usize,
) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if level != SOL_SOCKET || value == 0 || length == 0 {
        return -errno::ENOPROTOOPT;
    }
    let mut result = [0u8; 12];
    let result_length = match option {
        SO_TYPE => {
            let value: i32 = match socket.socket_type() {
                SocketType::Stream => 1,
                SocketType::Datagram => 2,
                SocketType::Raw => 3,
            };
            result[..4].copy_from_slice(&value.to_ne_bytes());
            4
        }
        SO_ERROR => {
            let value = socket
                .take_error()
                .map_or(0, |error| (-socket_error(error)) as i32);
            result[..4].copy_from_slice(&value.to_ne_bytes());
            4
        }
        SO_PEERCRED => {
            let credentials = match socket.peer_credentials() {
                Ok(credentials) => credentials,
                Err(error) => return socket_error(error),
            };
            result[..4].copy_from_slice(&credentials.pid.to_ne_bytes());
            result[4..8].copy_from_slice(&credentials.uid.to_ne_bytes());
            result[8..12].copy_from_slice(&credentials.gid.to_ne_bytes());
            12
        }
        _ => return -errno::ENOPROTOOPT,
    };
    let task = current_task().unwrap();
    let mut size = [0u8; 4];
    if task.copy_from_user(length, &mut size).is_err() {
        return -errno::EFAULT;
    }
    let count = (u32::from_ne_bytes(size) as usize).min(result_length);
    if task.copy_to_user(value, &result[..count]).is_err()
        || task
            .copy_to_user(length, &(result_length as u32).to_ne_bytes())
            .is_err()
    {
        return -errno::EFAULT;
    }
    0
}
