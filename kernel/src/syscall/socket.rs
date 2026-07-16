use alloc::sync::Arc;

use crate::{
    fs::{O_CLOEXEC, O_NONBLOCK, O_RDWR, OpenFileDescription, OpenFileKind},
    socket::{
        InetAddress, NetlinkAddress, PacketAddress, Socket, SocketAddress, SocketDomain,
        SocketError, SocketType, UnixAddress, UnixConnectResources, UnixCredentials,
        configure_address, configure_gateway, configure_netmask, configure_up, interface_snapshot,
    },
    task::{self, TaskControlBlock, WaitResult, current_task},
};

use super::{errno, poll::wait_for_ofd};

mod control;
mod interface;
mod message;
mod options;
mod unix_path;
pub(super) use interface::socket_ioctl;
pub(crate) use message::{sys_recvfrom, sys_recvmsg, sys_sendmsg, sys_sendto};
pub(crate) use options::{sys_getsockopt, sys_setsockopt};

const AF_UNIX: usize = 1;
const AF_INET: usize = 2;
const AF_PACKET: usize = 17;
const AF_NETLINK: usize = 16;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOCK_RAW: usize = 3;
const SOCK_CLOEXEC: usize = O_CLOEXEC as usize;
const SOCK_NONBLOCK: usize = O_NONBLOCK as usize;
const MSG_PEEK: usize = 0x2;
const MSG_TRUNC: usize = 0x20;
const MSG_DONTWAIT: usize = 0x40;
const MSG_NOSIGNAL: usize = 0x4000;
const IFNAMSIZ: usize = 16;
const IFREQ_SIZE: usize = 40;
const SIOCADDRT: usize = 0x890b;
const SIOCDELRT: usize = 0x890c;
const SIOCGIFNAME: usize = 0x8910;
const SIOCGIFCONF: usize = 0x8912;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const SIOCGIFADDR: usize = 0x8915;
const SIOCSIFADDR: usize = 0x8916;
const SIOCGIFBRDADDR: usize = 0x8919;
const SIOCSIFBRDADDR: usize = 0x891a;
const SIOCGIFNETMASK: usize = 0x891b;
const SIOCSIFNETMASK: usize = 0x891c;
const SIOCGIFMTU: usize = 0x8921;
const SIOCSIFMTU: usize = 0x8922;
const SIOCGIFHWADDR: usize = 0x8927;
const SIOCGIFINDEX: usize = 0x8933;
const IFF_UP: u16 = 0x1;
const IFF_BROADCAST: u16 = 0x2;
const IFF_RUNNING: u16 = 0x40;
const IFF_MULTICAST: u16 = 0x1000;
const ARPHRD_ETHER: u16 = 1;
const ETHERNET_MTU: i32 = 1500;
const INTERFACE_INDEX: i32 = 1;
const INTERFACE_NAME: &[u8] = b"eth0";

pub(super) fn socket_error(error: SocketError) -> isize {
    -match error {
        SocketError::Invalid | SocketError::WrongType => errno::EINVAL,
        SocketError::NoMemory => errno::ENOMEM,
        SocketError::AddressInUse => errno::EADDRINUSE,
        SocketError::AddressNotAvailable => errno::EADDRNOTAVAIL,
        SocketError::NotFound | SocketError::ConnectionRefused => errno::ECONNREFUSED,
        SocketError::ConnectionReset => errno::ECONNRESET,
        SocketError::NetworkUnreachable => errno::ENETUNREACH,
        SocketError::DestinationRequired => errno::EDESTADDRREQ,
        SocketError::MessageTooLarge => errno::EMSGSIZE,
        SocketError::ProtocolNotSupported => errno::EPROTONOSUPPORT,
        SocketError::OperationNotSupported => errno::EOPNOTSUPP,
        SocketError::NotConnected => errno::ENOTCONN,
        SocketError::AlreadyConnected => errno::EISCONN,
        SocketError::InProgress => errno::EINPROGRESS,
        SocketError::AlreadyInProgress => errno::EALREADY,
        SocketError::Again => errno::EAGAIN,
        SocketError::BrokenPipe => errno::EPIPE,
        SocketError::PermissionDenied => errno::EACCES,
        SocketError::NoDevice => errno::ENODEV,
        SocketError::TooManyReferences => errno::ETOOMANYREFS,
    }
}

fn decode_type(raw: usize) -> Result<(SocketType, u32, bool), isize> {
    if raw & !(0xf | SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(-errno::EINVAL);
    }
    let kind = match raw & 0xf {
        SOCK_STREAM => SocketType::Stream,
        SOCK_DGRAM => SocketType::Datagram,
        SOCK_RAW => SocketType::Raw,
        _ => return Err(-errno::ESOCKTNOSUPPORT),
    };
    Ok((
        kind,
        O_RDWR | (raw as u32 & O_NONBLOCK),
        raw & SOCK_CLOEXEC != 0,
    ))
}

fn new_socket(
    domain: SocketDomain,
    kind: SocketType,
    protocol: usize,
) -> Result<Arc<Socket>, isize> {
    let credentials = (domain == SocketDomain::Unix).then(current_unix_credentials);
    task::create_notification_endpoints()
        .map_err(|_| -errno::ENOMEM)
        .and_then(|notify| {
            Socket::new(domain, kind, protocol, notify, credentials).map_err(socket_error)
        })
}

fn current_unix_credentials() -> UnixCredentials {
    let task = current_task().expect("AF_UNIX operation requires current task");
    UnixCredentials {
        pid: task.tgid() as i32,
        uid: task.credential_id(true, true),
        gid: task.credential_id(false, true),
    }
}

fn socket_ofd(fd: usize) -> Result<(Arc<OpenFileDescription>, Arc<Socket>), isize> {
    let task = current_task().expect("socket syscall requires current task");
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    let OpenFileKind::Socket(socket) = &ofd.kind else {
        return Err(-errno::ENOTSOCK);
    };
    Ok((ofd.clone(), socket.clone()))
}

fn read_address(pointer: usize, length: usize) -> Result<SocketAddress, isize> {
    if pointer == 0 || !(2..=110).contains(&length) {
        return Err(-errno::EINVAL);
    }
    let task = current_task().unwrap();
    let mut bytes = [0u8; 110];
    task.copy_from_user(pointer, &mut bytes[..length])
        .map_err(|_| -errno::EFAULT)?;
    match u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize {
        AF_UNIX if length >= 3 => {
            let raw = &bytes[2..length];
            let path = if raw[0] == 0 {
                raw
            } else {
                let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
                &raw[..end]
            };
            UnixAddress::new(path)
                .map(SocketAddress::Unix)
                .map_err(socket_error)
        }
        AF_INET if length >= 16 => Ok(SocketAddress::Inet(InetAddress {
            address: core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[4..8]).unwrap()),
            port: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
        })),
        AF_PACKET if length >= 20 => Ok(SocketAddress::Packet(PacketAddress {
            protocol: u16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
            interface_index: i32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            hardware_type: u16::from_ne_bytes(bytes[8..10].try_into().unwrap()),
            packet_type: bytes[10],
            address_length: bytes[11],
            address: bytes[12..20].try_into().unwrap(),
        })),
        AF_NETLINK if length >= 12 => Ok(SocketAddress::Netlink(NetlinkAddress {
            port_id: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            groups: u32::from_ne_bytes(bytes[8..12].try_into().unwrap()),
        })),
        _ => Err(-errno::EAFNOSUPPORT),
    }
}

fn write_address(
    address: Option<SocketAddress>,
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
    let (encoded, actual) = encode_address(address);
    task.copy_to_user(pointer, &encoded[..actual.min(capacity)])
        .map_err(|_| -errno::EFAULT)?;
    task.copy_to_user(length_pointer, &(actual as u32).to_ne_bytes())
        .map_err(|_| -errno::EFAULT)
}

fn encode_address(address: Option<SocketAddress>) -> ([u8; 110], usize) {
    let mut encoded = [0u8; 110];
    let actual = match address {
        Some(SocketAddress::Unix(address)) => {
            encoded[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
            let count = address.bytes().len().min(108);
            encoded[2..2 + count].copy_from_slice(&address.bytes()[..count]);
            2 + count + usize::from(address.bytes().first() != Some(&0))
        }
        Some(SocketAddress::Inet(address)) => {
            encoded[..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
            encoded[2..4].copy_from_slice(&address.port.to_be_bytes());
            encoded[4..8].copy_from_slice(&address.address.octets());
            16
        }
        Some(SocketAddress::Packet(address)) => {
            encoded[..2].copy_from_slice(&(AF_PACKET as u16).to_ne_bytes());
            encoded[2..4].copy_from_slice(&address.protocol.to_ne_bytes());
            encoded[4..8].copy_from_slice(&address.interface_index.to_ne_bytes());
            encoded[8..10].copy_from_slice(&address.hardware_type.to_ne_bytes());
            encoded[10] = address.packet_type;
            encoded[11] = address.address_length;
            encoded[12..20].copy_from_slice(&address.address);
            20
        }
        Some(SocketAddress::Netlink(address)) => {
            encoded[..2].copy_from_slice(&(AF_NETLINK as u16).to_ne_bytes());
            encoded[4..8].copy_from_slice(&address.port_id.to_ne_bytes());
            encoded[8..12].copy_from_slice(&address.groups.to_ne_bytes());
            12
        }
        None => {
            encoded[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
            2
        }
    };
    (encoded, actual)
}

pub(crate) fn sys_socket(domain: usize, kind: usize, protocol: usize) -> isize {
    let domain = match domain {
        AF_UNIX => SocketDomain::Unix,
        AF_INET => SocketDomain::Inet,
        AF_PACKET => SocketDomain::Packet,
        AF_NETLINK => SocketDomain::Netlink,
        _ => return -errno::EAFNOSUPPORT,
    };
    let (kind, flags, cloexec) = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    // 当前没有 capability bitmap；effective UID 0 是 CAP_NET_RAW 的唯一标准等价策略。
    // 缺失该检查会允许普通用户创建 raw control/packet fd，绕过 L3 policy。
    if (domain == SocketDomain::Packet || kind == SocketType::Raw)
        && current_task().unwrap().credential_id(true, true) != 0
    {
        return -errno::EPERM;
    }
    let socket = match new_socket(domain, kind, protocol) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    let ofd = match OpenFileDescription::socket(socket, flags) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    current_task()
        .unwrap()
        .fd_allocate(ofd, cloexec)
        .map_or_else(super::file_descriptor_error, |fd| fd as isize)
}

pub(crate) fn sys_socketpair(domain: usize, kind: usize, protocol: usize, output: usize) -> isize {
    if domain != AF_UNIX || protocol != 0 || output == 0 {
        return -errno::EINVAL;
    }
    let (kind, flags, cloexec) = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let first = match new_socket(SocketDomain::Unix, kind, protocol) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    let second = match new_socket(SocketDomain::Unix, kind, protocol) {
        Ok(socket) => socket,
        Err(error) => return error,
    };
    let first_to_second = match task::create_pipe_endpoints() {
        Ok(pair) => pair,
        Err(_) => return -errno::ENOMEM,
    };
    let second_to_first = match task::create_pipe_endpoints() {
        Ok(pair) => pair,
        Err(_) => return -errno::ENOMEM,
    };
    if let Err(error) = Socket::pair(&first, &second, first_to_second, second_to_first) {
        return socket_error(error);
    }
    let task = current_task().unwrap();
    let first_ofd = match OpenFileDescription::socket(first, flags) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    let second_ofd = match OpenFileDescription::socket(second, flags) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    let (first_fd, second_fd) = match task.fd_allocate_pair(first_ofd, second_ofd, cloexec) {
        Ok(pair) => pair,
        Err(error) => return super::file_descriptor_error(error),
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
    let mut address = match read_address(address, length) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if let SocketAddress::Netlink(NetlinkAddress { port_id: 0, groups }) = address {
        let port_id = match u32::try_from(current_task().unwrap().tgid()) {
            Ok(port_id) => port_id,
            Err(_) => return -errno::EINVAL,
        };
        address = SocketAddress::Netlink(NetlinkAddress { port_id, groups });
    }
    let SocketAddress::Unix(unix) = &address else {
        return socket.bind(address).map_or_else(socket_error, |()| 0);
    };
    if unix.is_abstract() {
        return socket.bind(address).map_or_else(socket_error, |()| 0);
    }
    unix_path::bind(&socket, *unix)
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
    let (ofd, client) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let address = match read_address(address, length) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let resources =
        if client.domain() == SocketDomain::Unix && client.socket_type() == SocketType::Stream {
            let server_notify = match task::create_notification_endpoints() {
                Ok(value) => value,
                Err(_) => return -errno::ENOMEM,
            };
            let client_to_server = match task::create_pipe_endpoints() {
                Ok(value) => value,
                Err(_) => return -errno::ENOMEM,
            };
            let server_to_client = match task::create_pipe_endpoints() {
                Ok(value) => value,
                Err(_) => return -errno::ENOMEM,
            };
            Some(UnixConnectResources {
                server_notify,
                client_to_server,
                server_to_client,
            })
        } else {
            None
        };
    let credentials = (client.domain() == SocketDomain::Unix).then(current_unix_credentials);
    let unix_path = match &address {
        SocketAddress::Unix(unix) if !unix.is_abstract() => match unix_path::resolve(unix, true) {
            Ok(resolved) => Some(resolved),
            Err(error) => return error,
        },
        _ => None,
    };
    let unix_identity = unix_path.as_ref().map(|(_, identity)| *identity);
    match client.connect(address, resources, credentials, unix_identity) {
        Ok(()) => 0,
        Err(SocketError::InProgress) if *ofd.flags.lock() & O_NONBLOCK != 0 => -errno::EINPROGRESS,
        Err(SocketError::InProgress) => loop {
            match wait_for_ofd(&ofd, 4 | 8) {
                WaitResult::Woken => match client.connection_result() {
                    Ok(()) => return 0,
                    Err(SocketError::InProgress) => {}
                    Err(error) => return socket_error(error),
                },
                WaitResult::Interrupted => return -errno::EINTR,
                WaitResult::TimedOut => unreachable!(),
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            }
        },
        Err(error) => socket_error(error),
    }
}

pub(crate) fn sys_accept4(fd: usize, address: usize, length: usize, flags: usize) -> isize {
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    let (ofd, listener) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let accept_notify = if listener.domain() == SocketDomain::Inet {
        match task::create_notification_endpoints() {
            Ok(value) => Some(value),
            Err(_) => return -errno::ENOMEM,
        }
    } else {
        None
    };
    loop {
        match listener.accept_with_notify(accept_notify.clone()) {
            Ok(socket) => {
                let ofd = match OpenFileDescription::socket(
                    socket.clone(),
                    O_RDWR | (flags as u32 & O_NONBLOCK),
                ) {
                    Ok(ofd) => ofd,
                    Err(()) => return -errno::ENOMEM,
                };
                let result = current_task()
                    .unwrap()
                    .fd_allocate(ofd, flags & SOCK_CLOEXEC != 0);
                let fd = match result {
                    Ok(fd) => fd,
                    Err(error) => return super::file_descriptor_error(error),
                };
                if let Err(error) = socket
                    .peer_address()
                    .map_err(socket_error)
                    .and_then(|peer| write_address(peer, address, length))
                {
                    let _ = current_task().unwrap().fd_close(fd);
                    return error;
                }
                return fd as isize;
            }
            Err(SocketError::Again) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
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

pub(crate) fn sys_accept(fd: usize, address: usize, length: usize) -> isize {
    sys_accept4(fd, address, length, 0)
}

pub(crate) fn sys_getsockname(fd: usize, address: usize, length: usize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .address()
        .map_err(socket_error)
        .and_then(|value| write_address(value, address, length))
        .map_or_else(|error| error, |()| 0)
}

pub(crate) fn sys_getpeername(fd: usize, address: usize, length: usize) -> isize {
    let (_, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .peer_address()
        .map_err(socket_error)
        .and_then(|value| write_address(value, address, length))
        .map_or_else(|error| error, |()| 0)
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

fn encode_interface_name(request: &mut [u8; IFREQ_SIZE]) {
    request[..INTERFACE_NAME.len()].copy_from_slice(INTERFACE_NAME);
}

fn interface_name_matches(request: &[u8; IFREQ_SIZE]) -> bool {
    let end = request[..IFNAMSIZ]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    &request[..end] == INTERFACE_NAME
}

fn encode_inet_sockaddr(request: &mut [u8; IFREQ_SIZE], address: core::net::Ipv4Addr) {
    request[16..32].fill(0);
    request[16..18].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    request[20..24].copy_from_slice(&address.octets());
}

fn decode_inet_sockaddr(request: &[u8; IFREQ_SIZE]) -> Result<core::net::Ipv4Addr, isize> {
    if u16::from_ne_bytes(request[16..18].try_into().unwrap()) as usize != AF_INET {
        return Err(-errno::EAFNOSUPPORT);
    }
    Ok(core::net::Ipv4Addr::from(
        <[u8; 4]>::try_from(&request[20..24]).unwrap(),
    ))
}

fn netmask(prefix_length: u8) -> core::net::Ipv4Addr {
    let bits = if prefix_length == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_length)
    };
    core::net::Ipv4Addr::from(bits)
}

fn broadcast(address: core::net::Ipv4Addr, prefix_length: u8) -> core::net::Ipv4Addr {
    let mask = u32::from(netmask(prefix_length));
    core::net::Ipv4Addr::from(u32::from(address) | !mask)
}
