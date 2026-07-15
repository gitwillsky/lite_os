use alloc::{sync::Arc, vec::Vec};

use super::{
    MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK, MSG_TRUNC, O_NONBLOCK, SocketAddress, SocketError,
    TaskControlBlock, WaitResult, encode_address, errno, interface_snapshot, read_address,
    socket_error, socket_ofd, wait_for_ofd, write_address,
};
use crate::{
    fs::OpenFileDescription,
    socket::{Socket, SocketSendError},
    syscall::{
        poll::wait_for_socket_send,
        user_iovec::{
            BufferError, ImportError, UserIoCursor, UserIoVec, bounded_staging_capacity,
            import_iovecs, project_total_length, validate_user_buffers,
        },
    },
    task::{current_task, send_thread_signal},
};

const MESSAGE_HEADER_SIZE: usize = 56;
const STREAM_STAGING_BYTES: usize = 64 * 1024;
const SOCKET_MAX_RW_COUNT: usize = 0x7fff_f000;
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

fn import_message_iovecs(
    task: &TaskControlBlock,
    header: &MessageHeader,
) -> Result<(Vec<UserIoVec>, usize), isize> {
    let mut vectors =
        import_iovecs(task, header.iovecs, header.iovec_count).map_err(|error| match error {
            ImportError::TooMany | ImportError::NullArray => -errno::EINVAL,
            ImportError::AddressOverflow | ImportError::CopyFault => -errno::EFAULT,
            ImportError::NoMemory => -errno::ENOMEM,
        })?;
    // Linux 的单 entry fast path 先投影到 MAX_RW_COUNT 再 access_ok；multi-iovec
    // 则先验证每个原始 range，再截断越过传输上限的 suffix。
    let total = if vectors.len() == 1 {
        let total = project_total_length(&mut vectors, SOCKET_MAX_RW_COUNT);
        validate_user_buffers(&vectors).map_err(|error| match error {
            BufferError::NullBase | BufferError::AddressOverflow => -errno::EFAULT,
        })?;
        total
    } else {
        validate_user_buffers(&vectors).map_err(|error| match error {
            BufferError::NullBase | BufferError::AddressOverflow => -errno::EFAULT,
        })?;
        project_total_length(&mut vectors, SOCKET_MAX_RW_COUNT)
    };
    Ok((vectors, total))
}

fn message_buffer(length: usize) -> Result<Vec<u8>, isize> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| -errno::ENOMEM)?;
    bytes.resize(length, 0);
    Ok(bytes)
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

fn send_error(
    task: &TaskControlBlock,
    flags: usize,
    error: SocketError,
    completed: usize,
) -> isize {
    if error == SocketError::BrokenPipe && completed == 0 && flags & MSG_NOSIGNAL == 0 {
        send_thread_signal(task.tgid(), task.tid(), 13)
            .expect("current socket sender must remain live");
    }
    if completed == 0 {
        socket_error(error)
    } else {
        completed as isize
    }
}

fn send_one_message(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    socket: &Arc<Socket>,
    bytes: &[u8],
    target: Option<SocketAddress>,
    flags: usize,
) -> isize {
    let nonblocking = flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0;
    loop {
        match socket.send_to(bytes, target.clone()) {
            Ok(count) => return count as isize,
            Err(SocketSendError::WouldBlock | SocketSendError::PeerFull(_)) if nonblocking => {
                return -errno::EAGAIN;
            }
            Err(SocketSendError::WouldBlock) => match wait_for_ofd(ofd, 4) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return -errno::EINTR,
                WaitResult::TimedOut => unreachable!(),
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
            Err(SocketSendError::PeerFull(blocker)) => match wait_for_socket_send(&blocker) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return -errno::EINTR,
                WaitResult::TimedOut => unreachable!(),
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
            Err(SocketSendError::Error(error)) => return send_error(task, flags, error, 0),
        }
    }
}

fn send_stream_message(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    socket: &Arc<Socket>,
    vectors: &[UserIoVec],
    total_length: usize,
    target: Option<SocketAddress>,
    flags: usize,
) -> isize {
    if total_length == 0 {
        return send_one_message(task, ofd, socket, &[], target, flags);
    }
    let staging_capacity = socket
        .stream_send_staging_capacity(total_length, STREAM_STAGING_BYTES)
        .expect("stream send path must retain facade-selected staging policy");
    let mut staging = match message_buffer(staging_capacity) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let nonblocking = flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0;
    let mut cursor = UserIoCursor::new(vectors);
    while cursor.completed() < total_length {
        let capacity = bounded_staging_capacity(total_length - cursor.completed(), staging.len());
        let staged = cursor.stage_from_user(task, &mut staging[..capacity]);
        if staged.faulted {
            return if cursor.completed() == 0 {
                -errno::EFAULT
            } else {
                cursor.completed() as isize
            };
        }
        if staged.count == 0 {
            return if cursor.completed() == 0 {
                -errno::EFAULT
            } else {
                cursor.completed() as isize
            };
        }
        loop {
            match socket.send_to(&staging[..staged.count], target.clone()) {
                Ok(sent) => {
                    assert!(sent <= staged.count, "socket consumed beyond staged prefix");
                    cursor.advance(sent);
                    if sent < staged.count || sent == 0 {
                        return cursor.completed() as isize;
                    }
                    break;
                }
                Err(SocketSendError::WouldBlock | SocketSendError::PeerFull(_))
                    if cursor.completed() != 0 =>
                {
                    return cursor.completed() as isize;
                }
                Err(SocketSendError::WouldBlock | SocketSendError::PeerFull(_)) if nonblocking => {
                    return -errno::EAGAIN;
                }
                Err(SocketSendError::WouldBlock) => match wait_for_ofd(ofd, 4) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return -errno::EINTR,
                    WaitResult::TimedOut => unreachable!(),
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                },
                Err(SocketSendError::PeerFull(blocker)) => match wait_for_socket_send(&blocker) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return -errno::EINTR,
                    WaitResult::TimedOut => unreachable!(),
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                },
                Err(SocketSendError::Error(error)) => {
                    return send_error(task, flags, error, cursor.completed());
                }
            }
        }
    }
    cursor.completed() as isize
}

fn send_message(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    socket: &Arc<Socket>,
    vectors: &[UserIoVec],
    total_length: usize,
    target: Option<SocketAddress>,
    flags: usize,
) -> isize {
    if let Err(error) = socket.validate_send_length(total_length) {
        return socket_error(error);
    }
    if socket
        .stream_send_staging_capacity(total_length, STREAM_STAGING_BYTES)
        .is_some()
    {
        return send_stream_message(task, ofd, socket, vectors, total_length, target, flags);
    }
    let mut bytes = match message_buffer(total_length) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let mut cursor = UserIoCursor::new(vectors);
    if cursor.copy_from_user(task, &mut bytes).is_err() || cursor.completed() != total_length {
        return -errno::EFAULT;
    }
    send_one_message(task, ofd, socket, &bytes, target, flags)
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
    let (iovecs, total_length) = match import_message_iovecs(&task, &header) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let Err(error) = validate_send_control(&task, &header) {
        return error;
    }
    let target = if header.name == 0 {
        None
    } else {
        match read_address(header.name, header.name_length) {
            Ok(value) => Some(value),
            Err(error) => return error,
        }
    };
    send_message(&task, &ofd, &socket, &iovecs, total_length, target, flags)
}

pub(crate) fn sys_sendto(
    fd: usize,
    buffer: usize,
    length: usize,
    flags: usize,
    address: usize,
    address_length: usize,
) -> isize {
    if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let mut vectors = [UserIoVec {
        base: buffer,
        length,
    }];
    let length = project_total_length(&mut vectors, SOCKET_MAX_RW_COUNT);
    if validate_user_buffers(&vectors).is_err() {
        return -errno::EFAULT;
    }
    let target = if address == 0 {
        None
    } else {
        match read_address(address, address_length) {
            Ok(value) => Some(value),
            Err(error) => return error,
        }
    };
    send_message(&task, &ofd, &socket, &vectors, length, target, flags)
}

pub(crate) fn sys_recvfrom(
    fd: usize,
    buffer: usize,
    length: usize,
    flags: usize,
    address: usize,
    address_length: usize,
) -> isize {
    if flags & !(MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT) != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let mut vectors = [UserIoVec {
        base: buffer,
        length,
    }];
    let length = project_total_length(&mut vectors, SOCKET_MAX_RW_COUNT);
    if validate_user_buffers(&vectors).is_err() {
        return -errno::EFAULT;
    }
    let capacity = socket.receive_staging_capacity(length, STREAM_STAGING_BYTES);
    let mut output = match message_buffer(capacity) {
        Ok(output) => output,
        Err(error) => return error,
    };
    loop {
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0) {
            Ok(received) => {
                let mut cursor = UserIoCursor::new(&vectors);
                if cursor
                    .copy_to_user(&task, &output[..received.count])
                    .is_err()
                {
                    return -errno::EFAULT;
                }
                if let Err(error) = write_address(received.source, address, address_length) {
                    return error;
                }
                return if flags & MSG_TRUNC != 0 {
                    received.full_length as isize
                } else {
                    received.count as isize
                };
            }
            Err(SocketError::Again)
                if flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0 =>
            {
                return -errno::EAGAIN;
            }
            Err(SocketError::Again) => match wait_for_ofd(&ofd, 1) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return -errno::EINTR,
                WaitResult::TimedOut => unreachable!(),
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
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
    let (iovecs, total_length) = match import_message_iovecs(&task, &header) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let capacity = socket.receive_staging_capacity(total_length, STREAM_STAGING_BYTES);
    let mut output = match message_buffer(capacity) {
        Ok(output) => output,
        Err(error) => return error,
    };
    let nonblocking = flags & MSG_DONTWAIT != 0 || *ofd.flags.lock() & O_NONBLOCK != 0;
    loop {
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0) {
            Ok(received) => {
                let mut cursor = UserIoCursor::new(&iovecs);
                if cursor
                    .copy_to_user(&task, &output[..received.count])
                    .is_err()
                    || cursor.completed() != received.count
                {
                    return -errno::EFAULT;
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
            Err(SocketError::Again) => match wait_for_ofd(&ofd, 1) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return -errno::EINTR,
                WaitResult::TimedOut => unreachable!(),
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
            Err(error) => return socket_error(error),
        }
    }
}
