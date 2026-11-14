//! `SOL_PACKET` 的选项编码，套接字层拥有配置和映射寿命。

use super::{current_task, errno, read_i32, socket_error};
use crate::net::Socket;

const PACKET_RX_RING: usize = 5;
const PACKET_STATISTICS: usize = 6;
const PACKET_AUXDATA: usize = 8;
const PACKET_VERSION: usize = 10;
const PACKET_HDRLEN: usize = 11;
const PACKET_RESERVE: usize = 12;

/// 从用户空间读取标准 packet option 并交给 socket。
/// # Errors
/// 缓冲无效、选项不支持或 socket 拒绝变更，返回负 errno。
pub(super) fn set(
    socket: &Socket,
    option: usize,
    value: usize,
    length: usize,
) -> Result<(), isize> {
    match option {
        PACKET_AUXDATA => socket.set_packet_auxiliary(read_i32(value, length)? != 0),
        PACKET_VERSION | PACKET_RESERVE if length == 4 => {
            socket.set_packet_ring_option(option == PACKET_VERSION, read_i32(value, length)?)
        }
        PACKET_VERSION | PACKET_RESERVE => return Err(-errno::EINVAL),
        PACKET_RX_RING => {
            let mut bytes = [0; 16];
            if length < bytes.len() {
                return Err(-errno::EINVAL);
            }
            current_task()
                .unwrap()
                .copy_from_user(value, &mut bytes)
                .map_err(|_| -errno::EFAULT)?;
            let geometry = core::array::from_fn(|index| {
                let offset = index * 4;
                u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
            });
            socket.configure_packet_ring(geometry)
        }
        _ => return Err(-errno::ENOPROTOOPT),
    }
    .map_err(socket_error)
}

/// 编码 socket 的选项值，返回选项的完整字节数。
/// # Errors
/// 未知选项、PACKET_HDRLEN 输入无效或 socket 查询失败，返回负 errno。
pub(super) fn get(
    socket: &Socket,
    option: usize,
    value: usize,
    capacity: usize,
    result: &mut [u8; 16],
) -> Result<usize, isize> {
    let scalar = match option {
        PACKET_AUXDATA => i32::from(socket.packet_auxiliary()),
        PACKET_VERSION | PACKET_RESERVE => socket
            .packet_ring_option(option == PACKET_VERSION)
            .map_err(socket_error)?,
        PACKET_HDRLEN => {
            Socket::packet_header_length(read_i32(value, capacity)?).map_err(socket_error)?
        }
        PACKET_STATISTICS => {
            let [packets, drops] = socket.take_packet_statistics().map_err(socket_error)?;
            result[..4].copy_from_slice(&packets.to_ne_bytes());
            result[4..8].copy_from_slice(&drops.to_ne_bytes());
            return Ok(8);
        }
        _ => return Err(-errno::ENOPROTOOPT),
    };
    result[..4].copy_from_slice(&scalar.to_ne_bytes());
    Ok(4)
}
