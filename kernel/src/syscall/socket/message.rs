use alloc::vec::Vec;

use super::{
    MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK, MSG_TRUNC, O_NONBLOCK, SocketAddress, SocketError,
    TaskControlBlock, WaitResult, encode_address, errno, interface_snapshot, ofd_wait_keys,
    read_address, socket_error, socket_ofd, wait_for_poll,
};
use crate::task::current_task;

const MESSAGE_HEADER_SIZE: usize = 56;
const IOVEC_SIZE: usize = 16;
const MAX_IOVECS: usize = 1024;
const MAX_DATAGRAM_BYTES: usize = 65_535;
const MSG_CTRUNC: i32 = 0x8;
const IPPROTO_IP: i32 = 0;
const IP_PKTINFO: i32 = 8;
const CMSG_LENGTH: usize = 28;
const CMSG_SPACE: usize = 32;
const INTERFACE_INDEX: i32 = 1;

struct MessageHeader {
    name: usize,
    name_length: usize,
    iovecs: usize,
    iovec_count: usize,
    control: usize,
    control_length: usize,
}

fn read_header(task: &TaskControlBlock, pointer: usize) -> Result<MessageHeader, isize> {
    if pointer == 0 {
        return Err(-errno::EFAULT);
    }
    let mut bytes = [0u8; MESSAGE_HEADER_SIZE];
    task.copy_from_user(pointer, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    Ok(MessageHeader {
        name: usize::from_ne_bytes(bytes[..core::mem::size_of::<usize>()].try_into().unwrap()),
        name_length: u32::from_ne_bytes(bytes[8..12].try_into().unwrap()) as usize,
        iovecs: usize::from_ne_bytes(bytes[16..24].try_into().unwrap()),
        iovec_count: usize::from_ne_bytes(bytes[24..32].try_into().unwrap()),
        control: usize::from_ne_bytes(bytes[32..40].try_into().unwrap()),
        control_length: usize::from_ne_bytes(bytes[40..48].try_into().unwrap()),
    })
}

fn read_iovecs(
    task: &TaskControlBlock,
    header: &MessageHeader,
) -> Result<Vec<(usize, usize)>, isize> {
    if header.iovec_count > MAX_IOVECS || header.iovec_count != 0 && header.iovecs == 0 {
        return Err(-errno::EINVAL);
    }
    let mut iovecs = Vec::new();
    iovecs
        .try_reserve_exact(header.iovec_count)
        .map_err(|_| -errno::ENOMEM)?;
    let mut total = 0usize;
    for index in 0..header.iovec_count {
        let mut bytes = [0u8; IOVEC_SIZE];
        task.copy_from_user(header.iovecs + index * IOVEC_SIZE, &mut bytes)
            .map_err(|_| -errno::EFAULT)?;
        let base = usize::from_ne_bytes(bytes[..8].try_into().unwrap());
        let length = usize::from_ne_bytes(bytes[8..].try_into().unwrap());
        if length != 0 && base == 0 {
            return Err(-errno::EFAULT);
        }
        total = total.checked_add(length).ok_or(-errno::EMSGSIZE)?;
        if total > MAX_DATAGRAM_BYTES {
            return Err(-errno::EMSGSIZE);
        }
        iovecs.push((base, length));
    }
    Ok(iovecs)
}

fn total_length(iovecs: &[(usize, usize)]) -> usize {
    iovecs.iter().map(|(_, length)| *length).sum()
}

fn gather(task: &TaskControlBlock, iovecs: &[(usize, usize)]) -> Result<Vec<u8>, isize> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total_length(iovecs))
        .map_err(|_| -errno::ENOMEM)?;
    for (base, length) in iovecs {
        let start = bytes.len();
        bytes.resize(start + length, 0);
        task.copy_from_user(*base, &mut bytes[start..])
            .map_err(|_| -errno::EFAULT)?;
    }
    Ok(bytes)
}

fn scatter(task: &TaskControlBlock, iovecs: &[(usize, usize)], bytes: &[u8]) -> Result<(), isize> {
    let mut offset = 0;
    for (base, length) in iovecs {
        let count = (*length).min(bytes.len().saturating_sub(offset));
        if count != 0 {
            task.copy_to_user(*base, &bytes[offset..offset + count])
                .map_err(|_| -errno::EFAULT)?;
            offset += count;
        }
        if offset == bytes.len() {
            break;
        }
    }
    Ok(())
}

fn validate_send_control(task: &TaskControlBlock, header: &MessageHeader) -> Result<(), isize> {
    if header.control_length == 0 {
        return Ok(());
    }
    if header.control == 0 || header.control_length < CMSG_LENGTH {
        return Err(-errno::EINVAL);
    }
    let mut control = [0u8; CMSG_LENGTH];
    task.copy_from_user(header.control, &mut control)
        .map_err(|_| -errno::EFAULT)?;
    if usize::from_ne_bytes(control[..8].try_into().unwrap()) < CMSG_LENGTH
        || i32::from_ne_bytes(control[8..12].try_into().unwrap()) != IPPROTO_IP
        || i32::from_ne_bytes(control[12..16].try_into().unwrap()) != IP_PKTINFO
    {
        return Err(-errno::EOPNOTSUPP);
    }
    let requested = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&control[20..24]).unwrap());
    if !requested.is_unspecified()
        && interface_snapshot()
            .map_err(socket_error)?
            .address
            .is_none_or(|address| address != requested)
    {
        return Err(-errno::EADDRNOTAVAIL);
    }
    Ok(())
}

/// @description Linux sendmsg scatter/gather ABI，复用唯一 socket send path。
pub(crate) fn sys_sendmsg(fd: usize, message: usize, flags: usize) -> isize {
    if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let header = match read_header(&task, message) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let iovecs = match read_iovecs(&task, &header) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = validate_send_control(&task, &header) {
        return error;
    }
    let bytes = match gather(&task, &iovecs) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let target = if header.name == 0 {
        None
    } else {
        match read_address(header.name, header.name_length) {
            Ok(value) => Some(value),
            Err(error) => return error,
        }
    };
    let nonblocking = flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0;
    loop {
        match socket.send_to(&bytes, target.clone()) {
            Ok(count) => return count as isize,
            Err(SocketError::Again) if nonblocking => return -errno::EAGAIN,
            Err(SocketError::Again) => {
                match wait_for_poll(ofd_wait_keys(&ofd), None, || ofd.poll_events(4) != 0) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return -errno::EINTR,
                    WaitResult::TimedOut => unreachable!(),
                }
            }
            Err(error) => return socket_error(error),
        }
    }
}

fn write_receive_metadata(
    task: &TaskControlBlock,
    pointer: usize,
    header: &MessageHeader,
    source: Option<SocketAddress>,
    local: Option<core::net::Ipv4Addr>,
    packet_info: bool,
    truncated: bool,
) -> Result<(), isize> {
    let mut output_flags = if truncated { MSG_TRUNC as i32 } else { 0 };
    if header.name != 0 {
        let (encoded, actual) = encode_address(source);
        task.copy_to_user(header.name, &encoded[..actual.min(header.name_length)])
            .map_err(|_| -errno::EFAULT)?;
        task.copy_to_user(pointer + 8, &(actual as u32).to_ne_bytes())
            .map_err(|_| -errno::EFAULT)?;
    }
    let mut control_written = 0usize;
    if packet_info && let Some(local) = local {
        if header.control != 0 && header.control_length >= CMSG_SPACE {
            let mut control = [0u8; CMSG_SPACE];
            control[..8].copy_from_slice(&CMSG_LENGTH.to_ne_bytes());
            control[8..12].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
            control[12..16].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
            control[16..20].copy_from_slice(&INTERFACE_INDEX.to_ne_bytes());
            control[20..24].copy_from_slice(&local.octets());
            control[24..28].copy_from_slice(&local.octets());
            task.copy_to_user(header.control, &control)
                .map_err(|_| -errno::EFAULT)?;
            control_written = CMSG_SPACE;
        } else {
            output_flags |= MSG_CTRUNC;
        }
    }
    task.copy_to_user(pointer + 40, &control_written.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)?;
    task.copy_to_user(pointer + 48, &output_flags.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)
}

/// @description Linux recvmsg scatter/gather、MSG_PEEK 与 IPv4 PKTINFO ancillary ABI。
pub(crate) fn sys_recvmsg(fd: usize, message: usize, flags: usize) -> isize {
    if flags & !(MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT) != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let header = match read_header(&task, message) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let iovecs = match read_iovecs(&task, &header) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mut output = Vec::new();
    if output.try_reserve_exact(total_length(&iovecs)).is_err() {
        return -errno::ENOMEM;
    }
    output.resize(total_length(&iovecs), 0);
    let nonblocking = flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0;
    loop {
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0) {
            Ok(received) => {
                if let Err(error) = scatter(&task, &iovecs, &output[..received.count]) {
                    return error;
                }
                if let Err(error) = write_receive_metadata(
                    &task,
                    message,
                    &header,
                    received.source,
                    received.local_address,
                    socket.ipv4_packet_info(),
                    received.full_length > received.count,
                ) {
                    return error;
                }
                return if flags & MSG_TRUNC != 0 {
                    received.full_length as isize
                } else {
                    received.count as isize
                };
            }
            Err(SocketError::Again) if nonblocking => return -errno::EAGAIN,
            Err(SocketError::Again) => {
                match wait_for_poll(ofd_wait_keys(&ofd), None, || ofd.poll_events(1) != 0) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return -errno::EINTR,
                    WaitResult::TimedOut => unreachable!(),
                }
            }
            Err(error) => return socket_error(error),
        }
    }
}
