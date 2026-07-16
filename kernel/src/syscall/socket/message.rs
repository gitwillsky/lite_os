use alloc::{sync::Arc, vec::Vec};

use super::{
    MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK, MSG_TRUNC, O_NONBLOCK, SocketAddress, SocketError,
    TaskControlBlock, WaitResult, errno, read_address, socket_error, socket_ofd, wait_for_ofd,
    write_address,
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

struct SendContext<'a> {
    task: &'a TaskControlBlock,
    ofd: &'a Arc<OpenFileDescription>,
    socket: &'a Arc<Socket>,
    target: Option<SocketAddress>,
    flags: usize,
}

impl SendContext<'_> {
    fn nonblocking(&self) -> bool {
        self.flags & MSG_DONTWAIT != 0 || *self.ofd.flags.lock() & O_NONBLOCK != 0
    }
}

fn send_one_message(
    context: &SendContext<'_>,
    bytes: &[u8],
    rights: &mut Option<crate::socket::UnixRights>,
) -> isize {
    loop {
        match context
            .socket
            .send_to_with_rights(bytes, context.target.clone(), rights)
        {
            Ok(count) => return count as isize,
            Err(SocketSendError::WouldBlock | SocketSendError::PeerFull(_))
                if context.nonblocking() =>
            {
                return -errno::EAGAIN;
            }
            Err(SocketSendError::WouldBlock) => match wait_for_ofd(context.ofd, 4) {
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
                return send_error(context.task, context.flags, error, 0);
            }
        }
    }
}

fn send_stream_message(
    context: &SendContext<'_>,
    vectors: &[UserIoVec],
    total_length: usize,
    mut rights: Option<crate::socket::UnixRights>,
) -> isize {
    if total_length == 0 {
        return if rights.is_some() {
            -errno::EINVAL
        } else {
            send_one_message(context, &[], &mut rights)
        };
    }
    let staging_capacity = context
        .socket
        .stream_send_staging_capacity(total_length, STREAM_STAGING_BYTES)
        .expect("stream send path must retain facade-selected staging policy");
    let mut staging = match message_buffer(staging_capacity) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let mut cursor = UserIoCursor::new(vectors);
    while cursor.completed() < total_length {
        let capacity = bounded_staging_capacity(total_length - cursor.completed(), staging.len());
        let staged = cursor.stage_from_user(context.task, &mut staging[..capacity]);
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
            match context.socket.send_to_with_rights(
                &staging[..staged.count],
                context.target.clone(),
                &mut rights,
            ) {
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
                Err(SocketSendError::WouldBlock | SocketSendError::PeerFull(_))
                    if context.nonblocking() =>
                {
                    return -errno::EAGAIN;
                }
                Err(SocketSendError::WouldBlock) => match wait_for_ofd(context.ofd, 4) {
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
                    return send_error(context.task, context.flags, error, cursor.completed());
                }
            }
        }
    }
    cursor.completed() as isize
}

fn send_message(
    context: SendContext<'_>,
    vectors: &[UserIoVec],
    total_length: usize,
    rights: Option<crate::socket::UnixRights>,
) -> isize {
    if let Err(error) = context.socket.validate_send_length(total_length) {
        return socket_error(error);
    }
    if context
        .socket
        .stream_send_staging_capacity(total_length, STREAM_STAGING_BYTES)
        .is_some()
    {
        return send_stream_message(&context, vectors, total_length, rights);
    }
    let mut bytes = match message_buffer(total_length) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let mut cursor = UserIoCursor::new(vectors);
    if cursor.copy_from_user(context.task, &mut bytes).is_err()
        || cursor.completed() != total_length
    {
        return -errno::EFAULT;
    }
    let mut rights = rights;
    send_one_message(&context, &bytes, &mut rights)
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
    let rights =
        match super::control::parse_send(&task, &socket, header.control, header.control_length) {
            Ok(rights) => rights,
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
    send_message(
        SendContext {
            task: &task,
            ofd: &ofd,
            socket: &socket,
            target,
            flags,
        },
        &iovecs,
        total_length,
        rights,
    )
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
    send_message(
        SendContext {
            task: &task,
            ofd: &ofd,
            socket: &socket,
            target,
            flags,
        },
        &vectors,
        length,
        None,
    )
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
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0, false) {
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

/// @description Linux recvmsg scatter/gather、MSG_PEEK 与 IPv4 PKTINFO ancillary ABI。
pub(crate) fn sys_recvmsg(fd: usize, message: usize, flags: usize) -> isize {
    if flags & !(MSG_PEEK | MSG_TRUNC | MSG_DONTWAIT | super::control::MSG_CMSG_CLOEXEC) != 0 {
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
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0, true) {
            Ok(received) => {
                let mut cursor = UserIoCursor::new(&iovecs);
                if cursor
                    .copy_to_user(&task, &output[..received.count])
                    .is_err()
                    || cursor.completed() != received.count
                {
                    return -errno::EFAULT;
                }
                let target = super::control::ReceiveTarget {
                    task: &task,
                    message,
                    name: (header.name, header.name_length),
                    control: (header.control, header.control_length),
                };
                let content = super::control::ReceiveContent {
                    source: received.source,
                    local: received.local_address,
                    packet_info: socket.ipv4_packet_info(),
                    rights: received.rights,
                    cloexec: flags & super::control::MSG_CMSG_CLOEXEC != 0,
                    truncated: received.full_length > received.count,
                };
                if let Err(error) = super::control::write_receive(target, content) {
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
