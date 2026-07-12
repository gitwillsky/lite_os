use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{O_CLOEXEC, O_NONBLOCK, O_RDWR, OpenFileDescription, OpenFileKind},
    ipc::{SocketError, SocketType, UnixAddress, UnixSocket},
    task::{WaitResult, create_pipe_endpoints, current_task, wait_for_poll},
};

use super::{errno, poll::ofd_wait_keys};

const AF_UNIX: usize = 1;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_CLOEXEC: usize = O_CLOEXEC as usize;
const SOCK_NONBLOCK: usize = O_NONBLOCK as usize;

pub(super) fn socket_error(error: SocketError) -> isize {
    -match error {
        SocketError::Invalid | SocketError::WrongType => errno::EINVAL,
        SocketError::NoMemory => errno::ENOMEM,
        SocketError::AddressInUse => errno::EADDRINUSE,
        SocketError::NotFound | SocketError::ConnectionRefused => errno::ECONNREFUSED,
        SocketError::NotConnected => errno::ENOTCONN,
        SocketError::AlreadyConnected => errno::EISCONN,
        SocketError::Again => errno::EAGAIN,
        SocketError::BrokenPipe => errno::EPIPE,
    }
}

fn decode_type(raw: usize) -> Result<(SocketType, u32, bool), isize> {
    if raw & !(0xf | SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(-errno::EINVAL);
    }
    let kind = match raw & 0xf {
        SOCK_STREAM => SocketType::Stream,
        SOCK_DGRAM => SocketType::Datagram,
        _ => return Err(-errno::ESOCKTNOSUPPORT),
    };
    Ok((
        kind,
        O_RDWR | (raw as u32 & O_NONBLOCK),
        raw & SOCK_CLOEXEC != 0,
    ))
}

fn new_socket(kind: SocketType) -> Result<Arc<UnixSocket>, isize> {
    create_pipe_endpoints()
        .map(|notify| UnixSocket::new(kind, notify))
        .map_err(|_| -errno::ENOMEM)
}

fn socket_ofd(fd: usize) -> Result<(Arc<OpenFileDescription>, Arc<UnixSocket>), isize> {
    let task = current_task().expect("socket syscall requires current task");
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    let OpenFileKind::Socket(socket) = &ofd.kind else {
        return Err(-errno::ENOTSOCK);
    };
    Ok((ofd.clone(), socket.clone()))
}

fn read_address(pointer: usize, length: usize) -> Result<UnixAddress, isize> {
    if pointer == 0 || !(3..=110).contains(&length) {
        return Err(-errno::EINVAL);
    }
    let task = current_task().unwrap();
    let mut bytes = [0u8; 110];
    task.copy_from_user(pointer, &mut bytes[..length])
        .map_err(|_| -errno::EFAULT)?;
    if u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize != AF_UNIX {
        return Err(-errno::EAFNOSUPPORT);
    }
    let path = if bytes[2] == 0 {
        &bytes[2..length]
    } else {
        return Err(-errno::EOPNOTSUPP);
    };
    UnixAddress::new(path).map_err(socket_error)
}

fn write_address(
    address: Option<UnixAddress>,
    pointer: usize,
    length_pointer: usize,
) -> Result<(), isize> {
    if pointer == 0 {
        return Ok(());
    }
    if length_pointer == 0 {
        return Err(-errno::EFAULT);
    }
    let task = current_task().unwrap();
    let mut length_bytes = [0u8; 4];
    task.copy_from_user(length_pointer, &mut length_bytes)
        .map_err(|_| -errno::EFAULT)?;
    let capacity = u32::from_ne_bytes(length_bytes) as usize;
    let mut encoded = [0u8; 110];
    encoded[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    let actual = if let Some(address) = address {
        let count = address.bytes().len().min(108);
        encoded[2..2 + count].copy_from_slice(&address.bytes()[..count]);
        2 + count + usize::from(address.bytes().first() != Some(&0))
    } else {
        2
    };
    task.copy_to_user(pointer, &encoded[..actual.min(capacity)])
        .map_err(|_| -errno::EFAULT)?;
    task.copy_to_user(length_pointer, &(actual as u32).to_ne_bytes())
        .map_err(|_| -errno::EFAULT)
}

pub(crate) fn sys_socket(domain: usize, kind: usize, protocol: usize) -> isize {
    if domain != AF_UNIX {
        return -errno::EAFNOSUPPORT;
    }
    if protocol != 0 {
        return -errno::EPROTONOSUPPORT;
    }
    let (kind, flags, cloexec) = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let socket = match new_socket(kind) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    current_task()
        .unwrap()
        .fd_allocate(OpenFileDescription::socket(socket, flags), cloexec)
        .map_or(-errno::EMFILE, |fd| fd as isize)
}

pub(crate) fn sys_socketpair(domain: usize, kind: usize, protocol: usize, output: usize) -> isize {
    if domain != AF_UNIX || protocol != 0 || output == 0 {
        return -errno::EINVAL;
    }
    let (kind, flags, cloexec) = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let first = match new_socket(kind) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    let second = match new_socket(kind) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    let first_to_second = match create_pipe_endpoints() {
        Ok(pair) => pair,
        Err(_) => return -errno::ENOMEM,
    };
    let second_to_first = match create_pipe_endpoints() {
        Ok(pair) => pair,
        Err(_) => return -errno::ENOMEM,
    };
    if let Err(error) = UnixSocket::pair(&first, &second, first_to_second, second_to_first) {
        return socket_error(error);
    }
    let task = current_task().unwrap();
    let (first_fd, second_fd) = match task.fd_allocate_pair(
        OpenFileDescription::socket(first, flags),
        OpenFileDescription::socket(second, flags),
        cloexec,
    ) {
        Ok(pair) => pair,
        Err(_) => return -errno::EMFILE,
    };
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(first_fd as i32).to_ne_bytes());
    bytes[4..].copy_from_slice(&(second_fd as i32).to_ne_bytes());
    if task.copy_to_user(output, &bytes).is_err() {
        let _ = task.fd_close(first_fd);
        let _ = task.fd_close(second_fd);
        return -errno::EFAULT;
    }
    0
}

pub(crate) fn sys_bind(fd: usize, address: usize, length: usize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let address = match read_address(address, length) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket.bind(address).map_or_else(socket_error, |()| 0)
}

pub(crate) fn sys_listen(fd: usize, backlog: isize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .listen(backlog.max(0) as usize)
        .map_or_else(socket_error, |()| 0)
}

pub(crate) fn sys_connect(fd: usize, address: usize, length: usize) -> isize {
    let (_, client) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let address = match read_address(address, length) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let listener = match UnixSocket::lookup(&address) {
        Ok(value) => value,
        Err(error) => return socket_error(error),
    };
    if client.socket_type() == SocketType::Datagram {
        return client
            .connect_datagram(&listener)
            .map_or_else(socket_error, |()| 0);
    }
    let server = match new_socket(SocketType::Stream) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let first = match create_pipe_endpoints() {
        Ok(value) => value,
        Err(_) => return -errno::ENOMEM,
    };
    let second = match create_pipe_endpoints() {
        Ok(value) => value,
        Err(_) => return -errno::ENOMEM,
    };
    UnixSocket::connect_stream(&client, &listener, server, first, second)
        .map_or_else(socket_error, |()| 0)
}

pub(crate) fn sys_accept4(fd: usize, address: usize, length: usize, flags: usize) -> isize {
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    let (ofd, listener) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    loop {
        match listener.accept() {
            Ok(socket) => {
                let result = current_task().unwrap().fd_allocate(
                    OpenFileDescription::socket(
                        socket.clone(),
                        O_RDWR | (flags as u32 & O_NONBLOCK),
                    ),
                    flags & SOCK_CLOEXEC != 0,
                );
                let fd = match result {
                    Ok(fd) => fd,
                    Err(_) => return -errno::EMFILE,
                };
                if let Err(error) = write_address(socket.peer_address(), address, length) {
                    let _ = current_task().unwrap().fd_close(fd);
                    return error;
                }
                return fd as isize;
            }
            Err(SocketError::Again) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                return -errno::EAGAIN;
            }
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

pub(crate) fn sys_accept(fd: usize, address: usize, length: usize) -> isize {
    sys_accept4(fd, address, length, 0)
}

pub(crate) fn sys_getsockname(fd: usize, address: usize, length: usize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    write_address(socket.address(), address, length).map_or_else(|error| error, |()| 0)
}

pub(crate) fn sys_getpeername(fd: usize, address: usize, length: usize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if !socket.poll_state().writable {
        return -errno::ENOTCONN;
    }
    write_address(socket.peer_address(), address, length).map_or_else(|error| error, |()| 0)
}

pub(crate) fn sys_sendto(
    fd: usize,
    buffer: usize,
    length: usize,
    flags: usize,
    address: usize,
    address_length: usize,
) -> isize {
    if flags != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mut input = Vec::new();
    if input.try_reserve_exact(length).is_err() {
        return -errno::ENOMEM;
    }
    input.resize(length, 0);
    if current_task()
        .unwrap()
        .copy_from_user(buffer, &mut input)
        .is_err()
    {
        return -errno::EFAULT;
    }
    let target = if address == 0 {
        None
    } else {
        let decoded = match read_address(address, address_length) {
            Ok(value) => value,
            Err(error) => return error,
        };
        match UnixSocket::lookup(&decoded) {
            Ok(value) => Some(value),
            Err(error) => return socket_error(error),
        }
    };
    socket
        .send_to(&input, target.as_ref())
        .map_or_else(socket_error, |count| count as isize)
}

pub(crate) fn sys_recvfrom(
    fd: usize,
    buffer: usize,
    length: usize,
    flags: usize,
    address: usize,
    address_length: usize,
) -> isize {
    if flags != 0 {
        return -errno::EOPNOTSUPP;
    }
    let (ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let mut output = Vec::new();
    if output.try_reserve_exact(length).is_err() {
        return -errno::ENOMEM;
    }
    output.resize(length, 0);
    loop {
        match socket.receive(&mut output) {
            Ok((count, source)) => {
                if current_task()
                    .unwrap()
                    .copy_to_user(buffer, &output[..count])
                    .is_err()
                {
                    return -errno::EFAULT;
                }
                if let Err(error) = write_address(source, address, address_length) {
                    return error;
                }
                return count as isize;
            }
            Err(SocketError::Again) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                return -errno::EAGAIN;
            }
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

pub(crate) fn sys_shutdown(fd: usize, how: usize) -> isize {
    if how > 2 {
        return -errno::EINVAL;
    }
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket.shutdown(how).map_or_else(socket_error, |()| 0)
}

pub(crate) fn sys_setsockopt(
    fd: usize,
    level: usize,
    option: usize,
    _value: usize,
    length: usize,
) -> isize {
    if socket_ofd(fd).is_err() {
        return -errno::ENOTSOCK;
    }
    if level == 1 && matches!(option, 2 | 7 | 8) && length == 4 {
        0
    } else {
        -errno::ENOPROTOOPT
    }
}

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
    if level != 1 || value == 0 || length == 0 {
        return -errno::ENOPROTOOPT;
    }
    let result: i32 = match option {
        3 => match socket.socket_type() {
            SocketType::Stream => 1,
            SocketType::Datagram => 2,
        },
        4 => 0,
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
