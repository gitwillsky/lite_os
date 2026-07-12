use super::{SocketType, current_task, errno, socket_error, socket_ofd};

const IPPROTO_IP: usize = 0;
const IP_PKTINFO: usize = 8;
const SOL_SOCKET: usize = 1;
const SO_TYPE: usize = 3;
const SO_ERROR: usize = 4;

/// @description 设置当前已实现的 Linux socket option；未实现 option 明确返回 ENOPROTOOPT。
///
/// @param fd socket descriptor。
/// @param level Linux option level；当前只接受 `IPPROTO_IP`。
/// @param option option number；当前只接受 `IP_PKTINFO`。
/// @param value 指向 32-bit enabled value 的 userspace pointer。
/// @param length value buffer 长度，必须为 4。
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
    if level != IPPROTO_IP || option != IP_PKTINFO || length != 4 {
        return -errno::ENOPROTOOPT;
    }
    let mut enabled = [0u8; 4];
    if value == 0
        || current_task()
            .unwrap()
            .copy_from_user(value, &mut enabled)
            .is_err()
    {
        return -errno::EFAULT;
    }
    socket
        .set_ipv4_packet_info(i32::from_ne_bytes(enabled) != 0)
        .map_or_else(socket_error, |()| 0)
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
    let result: i32 = match option {
        SO_TYPE => match socket.socket_type() {
            SocketType::Stream => 1,
            SocketType::Datagram => 2,
        },
        SO_ERROR => 0,
        _ => return -errno::ENOPROTOOPT,
    };
    let task = current_task().unwrap();
    let mut size = [0u8; 4];
    if task.copy_from_user(length, &mut size).is_err() {
        return -errno::EFAULT;
    }
    let count = (u32::from_ne_bytes(size) as usize).min(4);
    if task
        .copy_to_user(value, &result.to_ne_bytes()[..count])
        .is_err()
        || task.copy_to_user(length, &4u32.to_ne_bytes()).is_err()
    {
        return -errno::EFAULT;
    }
    0
}
