use crate::sync::WaitResult;
use alloc::sync::Arc;

use crate::{
    file::{OpenFileDescription, OpenFileKind, PreparedSocketDescription},
    fs::{O_CLOEXEC, O_NONBLOCK, O_RDWR},
    socket::{
        InetAddress, NetlinkAddress, PacketAddress, Socket, SocketAddress, SocketConnectError,
        SocketDomain, SocketError, SocketRequest, SocketType, UnixAddress, UnixConnectResources,
        UnixConnectionData, UnixCredentials, configure_address, configure_broadcast, configure_mtu,
        configure_netmask, configure_route, configure_up, interface_index, interface_snapshot,
        interface_snapshots,
    },
    task::{TaskControlBlock, current_task},
};

use super::{errno, poll::wait_for_ofd};
use crate::socket::SocketIoDirection;

mod control;
mod control_list;
mod credentials;
pub(in crate::syscall) use message::write_vectors;
mod interface;
mod listen;
mod message;
mod options;
mod timeout;
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
const SOCK_SEQPACKET: usize = 5;
pub(in crate::syscall) const SOCK_CLOEXEC: usize = O_CLOEXEC as usize;
pub(in crate::syscall) const SOCK_NONBLOCK: usize = O_NONBLOCK as usize;
const MSG_PEEK: usize = 0x2;
const MSG_TRUNC: usize = 0x20;
pub(in crate::syscall) const MSG_DONTWAIT: usize = 0x40;
pub(in crate::syscall) const MSG_NOSIGNAL: usize = 0x4000;
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
const ARPHRD_ETHER: u16 = 1;

/// Linux socket 等待被 signal 打断时，无限等待允许 SA_RESTART，设置超时则直接 EINTR。
/// 调用者必须传本次操作已冻结的 deadline，不能读取后来改变的 option。
pub(in crate::syscall) fn interrupted_wait(deadline: Option<u64>) -> isize {
    if deadline.is_some() {
        -errno::EINTR
    } else {
        super::INTERNAL_RESTART_SYS
    }
}

pub(super) fn socket_error(error: SocketError) -> isize {
    -match error {
        SocketError::Invalid | SocketError::WrongType => errno::EINVAL,
        SocketError::NoMemory => errno::ENOMEM,
        SocketError::NoBuffer => errno::ENOBUFS,
        SocketError::NoSpace => errno::ENOSPC,
        SocketError::AddressInUse => errno::EADDRINUSE,
        SocketError::AddressNotAvailable => errno::EADDRNOTAVAIL,
        SocketError::ConnectionRefused => errno::ECONNREFUSED,
        SocketError::ConnectionReset => errno::ECONNRESET,
        SocketError::ConnectionAborted => errno::ECONNABORTED,
        SocketError::TimedOut => errno::ETIMEDOUT,
        SocketError::Device => errno::EIO,
        SocketError::NetworkUnreachable => errno::ENETUNREACH,
        SocketError::HostUnreachable => errno::EHOSTUNREACH,
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
        SocketError::OperationNotPermitted => errno::EPERM,
        SocketError::NoDevice => errno::ENODEV,
        SocketError::DeviceNotAvailable => errno::ENXIO,
        SocketError::NetworkDown => errno::ENETDOWN,
        SocketError::TooManyReferences => errno::ETOOMANYREFS,
    }
}

fn decode_flags(raw: usize) -> Result<(usize, u32, bool), isize> {
    if raw & !(0xf | SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(-errno::EINVAL);
    }
    Ok((
        raw & 0xf,
        O_RDWR | (raw as u32 & O_NONBLOCK),
        raw & SOCK_CLOEXEC != 0,
    ))
}
fn decode_domain(raw: usize) -> Result<SocketDomain, isize> {
    match raw {
        AF_UNIX => Ok(SocketDomain::Unix),
        AF_INET => Ok(SocketDomain::Inet),
        AF_PACKET => Ok(SocketDomain::Packet),
        AF_NETLINK => Ok(SocketDomain::Netlink),
        _ => Err(-errno::EAFNOSUPPORT),
    }
}
fn decode_type(raw: usize) -> Result<SocketType, isize> {
    match raw {
        SOCK_STREAM => Ok(SocketType::Stream),
        SOCK_DGRAM => Ok(SocketType::Datagram),
        SOCK_RAW => Ok(SocketType::Raw),
        SOCK_SEQPACKET => Ok(SocketType::SeqPacket),
        _ => Err(-errno::ESOCKTNOSUPPORT),
    }
}

fn unix_credentials(task: &TaskControlBlock) -> UnixCredentials {
    let [uid, gid] = task.message_identity().ids(true);
    UnixCredentials {
        pid: task.tgid_identity().clone(),
        uid,
        gid,
    }
}

/// 取 OFD 背后的 socket；不是 socket 返回 `ENOTSOCK`。
pub(in crate::syscall) fn socket_of(ofd: &OpenFileDescription) -> Result<Arc<Socket>, isize> {
    match &ofd.kind {
        OpenFileKind::Socket(socket) => Ok(socket.clone()),
        _ => Err(-errno::ENOTSOCK),
    }
}

fn socket_ofd(fd: usize) -> Result<(Arc<OpenFileDescription>, Arc<Socket>), isize> {
    let task = current_task().expect("socket syscall requires current task");
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    let socket = socket_of(&ofd)?;
    Ok((ofd, socket))
}

// 仅 bind 接受 family-only 地址；connect/send 的未命名目的地址按 Linux 返回 EINVAL。
fn copy_address(
    task: &TaskControlBlock,
    pointer: usize,
    length: usize,
) -> Result<[u8; 128], isize> {
    if !(2..=128).contains(&length) {
        return Err(-errno::EINVAL);
    }
    let mut bytes = [0u8; 128];
    task.copy_from_user(pointer, &mut bytes[..length])
        .map_err(|_| -errno::EFAULT)?;
    Ok(bytes)
}

fn read_address(
    task: &TaskControlBlock,
    pointer: usize,
    length: usize,
    allow_unnamed: bool,
) -> Result<SocketAddress, isize> {
    let bytes = copy_address(task, pointer, length)?;
    decode_address(&bytes[..length], allow_unnamed)
}

fn decode_address(bytes: &[u8], allow_unnamed: bool) -> Result<SocketAddress, isize> {
    let length = bytes.len();
    match u16::from_ne_bytes(bytes[..2].try_into().unwrap()) as usize {
        AF_UNIX if length <= 110 => {
            let raw = &bytes[2..length];
            if raw.is_empty() && !allow_unnamed {
                return Err(-errno::EINVAL);
            }
            let path = if raw.first().is_none_or(|byte| *byte == 0) {
                raw
            } else {
                let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
                &raw[..end]
            };
            UnixAddress::new(path)
                .map(SocketAddress::Unix)
                .map_err(socket_error)
        }
        AF_UNIX => Err(-errno::EINVAL),
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

pub(in crate::syscall) fn write_address(
    task: &TaskControlBlock,
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

// Linux 非 NUL 结尾的完整 108-byte pathname 输入，在 getsockname 输出追加一个 NUL。
// 输出 storage 必须容纳 family + path + NUL；否则容量足够的 copyout 会越过 array 边界。
const UNIX_ADDRESS_OUTPUT_BYTES: usize = 2 + 108 + 1;
fn encode_address(address: Option<SocketAddress>) -> ([u8; UNIX_ADDRESS_OUTPUT_BYTES], usize) {
    let mut encoded = [0u8; UNIX_ADDRESS_OUTPUT_BYTES];
    let actual = match address {
        Some(SocketAddress::Unix(address)) => {
            encoded[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
            let count = address.bytes().len().min(108);
            encoded[2..2 + count].copy_from_slice(&address.bytes()[..count]);
            2 + count + usize::from(!address.is_unnamed() && !address.is_abstract())
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
        // 没有 source（stream/UDP EOF）不等于 AF_UNIX 未命名地址，不能伪造 family。
        None => 0,
    };
    (encoded, actual)
}

pub(crate) fn sys_socket(domain: usize, kind: usize, protocol: usize) -> isize {
    let (kind, flags, cloexec) = match decode_flags(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let domain = match decode_domain(domain) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let kind = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let net = task.net_namespace();
    // Packet create先授权，INET raw在protocol lookup之后授权；均检查实际NET的owning UserNS。
    // initial/global capability不能替代新NET的能力，也不能把普通NETLINK RAW误判为IP raw。
    if domain == SocketDomain::Packet
        && !task.has_namespace_capability(
            &net.user_namespace(),
            syscall_abi::capability::Capability::NET_RAW,
        )
    {
        return -errno::EPERM;
    }
    let request = match SocketRequest::decode(domain, kind, protocol) {
        Ok(request) => request,
        Err(error) => return socket_error(error),
    };
    if domain == SocketDomain::Inet
        && kind == SocketType::Raw
        && !task.has_namespace_capability(
            &net.user_namespace(),
            syscall_abi::capability::Capability::NET_RAW,
        )
    {
        return -errno::EPERM;
    }
    let prepared = match PreparedSocketDescription::try_new(flags, &task.access_identity(true), net)
    {
        Ok(prepared) => prepared,
        Err(_) => return -errno::ENOMEM,
    };
    let notify = match crate::ipc::Pipe::notification_pair() {
        Ok(notify) => notify,
        Err(_) => return -errno::ENOMEM,
    };
    let reservation = match task.fd_prepare_open(cloexec) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    match prepared.create(request, notify) {
        Ok(ofd) => reservation.publish(ofd) as isize,
        Err(error) => socket_error(error),
    }
}

/// Typed producer seam；stream 构造 direction edge notifications，payload 在实际发送时分配。
fn unix_connection_data(kind: SocketType) -> Result<UnixConnectionData, SocketError> {
    if kind != SocketType::Stream {
        return Ok(UnixConnectionData::Packet);
    }
    let first_to_second =
        crate::ipc::Pipe::notification_pair().map_err(|_| SocketError::NoMemory)?;
    let second_to_first =
        crate::ipc::Pipe::notification_pair().map_err(|_| SocketError::NoMemory)?;
    Ok(UnixConnectionData::Stream {
        first_to_second,
        second_to_first,
    })
}

pub(crate) fn sys_socketpair(domain: usize, kind: usize, protocol: usize, output: usize) -> isize {
    let (kind, flags, cloexec) = match decode_flags(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let reservation = match task.fd_prepare_opens::<2>(cloexec) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    let [first_fd, second_fd] = reservation.descriptors();
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(first_fd as i32).to_ne_bytes());
    bytes[4..].copy_from_slice(&(second_fd as i32).to_ne_bytes());
    if task.copy_to_user(output, &bytes).is_err() {
        return -errno::EFAULT;
    }
    // Linux socketpair 在 socket 创建之前预留 fd 并完成 number copyout；失败仍不可见。
    let domain = match decode_domain(domain) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let kind = match decode_type(kind) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let request = match SocketRequest::decode(domain, kind, protocol) {
        Ok(request) => request,
        Err(error) => return socket_error(error),
    };
    if domain != SocketDomain::Unix {
        return -errno::EOPNOTSUPP;
    }
    let create = || {
        let prepared = PreparedSocketDescription::try_new(
            flags,
            &task.access_identity(true),
            task.net_namespace(),
        )
        .map_err(|_| -errno::ENOMEM)?;
        let notify = crate::ipc::Pipe::notification_pair().map_err(|_| -errno::ENOMEM)?;
        prepared.create(request, notify).map_err(socket_error)
    };
    let first_ofd = match create() {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    let second_ofd = match create() {
        Ok(ofd) => ofd,
        Err(error) => return error,
    };
    let (OpenFileKind::Socket(first), OpenFileKind::Socket(second)) =
        (&first_ofd.kind, &second_ofd.kind)
    else {
        unreachable!("prepared socket description requires socket backend")
    };
    let data = match unix_connection_data(kind) {
        Ok(data) => data,
        Err(error) => return socket_error(error),
    };
    if let Err(error) = Socket::pair(first, second, data, unix_credentials(&task)) {
        return socket_error(error);
    }
    reservation.publish_many([first_ofd, second_ofd]);
    0
}

pub(crate) fn sys_bind(fd: usize, address: usize, length: usize) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let address = match read_address(&task, address, length, true) {
        Ok(value) => value,
        Err(error) => return error,
    };
    // 自身PID namespace的可见TGID只是autobind输入；actual NET owner决定碰撞与最终port。
    let caller_tgid = task.visible_tgid();
    let SocketAddress::Unix(unix) = &address else {
        return socket
            .bind(address, caller_tgid)
            .map_or_else(socket_error, |()| 0);
    };
    if unix.is_abstract() || unix.is_unnamed() {
        return socket
            .bind(address, caller_tgid)
            .map_or_else(socket_error, |()| 0);
    }
    unix_path::bind(&socket, *unix)
}

pub(crate) fn sys_listen(fd: usize, backlog: isize) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .listen(
            listen::normalize_backlog(backlog),
            (socket.domain() == SocketDomain::Unix)
                .then(|| unix_credentials(&current_task().unwrap())),
        )
        .map_or_else(socket_error, |()| 0)
}

/// 一次 connect 尝试的结果。
pub(in crate::syscall) enum ConnectBegin {
    Connected,
    /// 握手进行中：等待 socket 可写或出错，再读 [`Socket::connection_result`]。
    InProgress,
    /// Unix 目标的 backlog 已满：可等待的容量句柄。
    BacklogFull(crate::socket::SocketCapacityWait),
}

/// 发起 connect（不等待）：解码地址、解析 Unix pathname、提交到后端。
///
/// # Returns
///
/// 已连接、握手进行中或 backlog 已满；地址、权限或后端错误返回负 errno。
pub(in crate::syscall) fn connect_begin(
    task: &TaskControlBlock,
    client: &Arc<Socket>,
    address: usize,
    length: usize,
) -> Result<ConnectBegin, isize> {
    let bytes = copy_address(task, address, length)?;
    if u16::from_ne_bytes(bytes[..2].try_into().unwrap()) == 0 {
        return client
            .disconnect()
            .map(|_| ConnectBegin::Connected)
            .map_err(socket_error);
    }
    let address = decode_address(&bytes[..length], false)?;
    let credentials = (client.domain() == SocketDomain::Unix).then(|| unix_credentials(task));
    let resources = || {
        let server_notify = match crate::ipc::Pipe::notification_pair() {
            Ok(value) => value,
            Err(_) => return Err(SocketError::NoMemory),
        };
        Ok(UnixConnectResources {
            server_notify,
            data: unix_connection_data(client.socket_type())?,
        })
    };
    loop {
        let unix_path = match &address {
            SocketAddress::Unix(unix) if !unix.is_abstract() => {
                Some(unix_path::resolve(task, unix, true)?)
            }
            _ => None,
        };
        let unix_identity = unix_path.as_ref().map(|(_, identity)| *identity);
        match client.connect(
            address.clone(),
            credentials.clone(),
            unix_identity,
            resources,
        ) {
            Ok(()) => return Ok(ConnectBegin::Connected),
            Err(SocketConnectError::DestinationChanged) => continue,
            Err(SocketConnectError::BacklogFull(capacity)) => {
                return Ok(ConnectBegin::BacklogFull(capacity));
            }
            Err(SocketConnectError::Error(SocketError::InProgress)) => {
                return Ok(ConnectBegin::InProgress);
            }
            Err(SocketConnectError::Error(error)) => return Err(socket_error(error)),
        }
    }
}

pub(crate) fn sys_connect(fd: usize, address: usize, length: usize) -> isize {
    let (ofd, client) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let task = current_task().unwrap();
    let deadline = client
        .timeout(SocketIoDirection::Send)
        .deadline(crate::timer::get_time_ns());
    let nonblocking = ofd.status_flags() & O_NONBLOCK != 0;
    loop {
        match connect_begin(&task, &client, address, length) {
            Ok(ConnectBegin::Connected) => return 0,
            Err(error) => return error,
            Ok(ConnectBegin::BacklogFull(_)) if nonblocking => return -errno::EAGAIN,
            Ok(ConnectBegin::BacklogFull(capacity)) => {
                match super::poll::wait_for_socket_capacity(&capacity, deadline) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return interrupted_wait(deadline),
                    WaitResult::TimedOut => return -errno::EAGAIN,
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                }
            }
            Ok(ConnectBegin::InProgress) if nonblocking => return -errno::EINPROGRESS,
            Ok(ConnectBegin::InProgress) => loop {
                match wait_for_ofd(&ofd, 4 | 8, deadline) {
                    WaitResult::Woken => match client.connection_result() {
                        Ok(()) => return 0,
                        Err(SocketError::InProgress) => {}
                        Err(error) => return socket_error(error),
                    },
                    WaitResult::Interrupted => return interrupted_wait(deadline),
                    WaitResult::TimedOut => return -errno::EINPROGRESS,
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                }
            },
        }
    }
}

/// 尝试接受一个连接（不等待）：没有待接受的连接返回 `-EAGAIN`。
///
/// 每次尝试都重新预留 fd 槽与 socket 描述，所以重试之间不持有 task 的 fd 表状态；
/// 成功时把新 fd 发布到 `task` 的 fd 表并写回对端地址。
pub(in crate::syscall) fn accept_once(
    task: &TaskControlBlock,
    listener: &Arc<Socket>,
    flags: usize,
    address: usize,
    length: usize,
) -> isize {
    let reservation = match task.fd_prepare_open(flags & SOCK_CLOEXEC != 0) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    let prepared = match PreparedSocketDescription::try_new(
        O_RDWR | (flags as u32 & O_NONBLOCK),
        &task.access_identity(true),
        listener.net_namespace(),
    ) {
        Ok(prepared) => prepared,
        Err(_) => return -errno::ENOMEM,
    };
    let accept_notify = if listener.domain() == SocketDomain::Inet {
        match crate::ipc::Pipe::notification_pair() {
            Ok(value) => Some(value),
            Err(_) => return -errno::ENOMEM,
        }
    } else {
        None
    };
    match prepared.accept(listener, accept_notify) {
        Ok(accepted) => {
            let OpenFileKind::Socket(socket) = &accepted.kind else {
                unreachable!()
            };
            if let Err(error) = socket
                .peer_address()
                .map_err(socket_error)
                .and_then(|peer| write_address(task, peer, address, length))
            {
                return error;
            }
            reservation.publish(accepted) as isize
        }
        Err((error, _)) => socket_error(error),
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
    let task = current_task().unwrap();
    let deadline = listener
        .timeout(SocketIoDirection::Receive)
        .deadline(crate::timer::get_time_ns());
    let nonblocking = ofd.status_flags() & O_NONBLOCK != 0;
    loop {
        let result = accept_once(&task, &listener, flags, address, length);
        if result != -errno::EAGAIN || nonblocking {
            return result;
        }
        match wait_for_ofd(&ofd, 1, deadline) {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return interrupted_wait(deadline),
            WaitResult::TimedOut => return -errno::EAGAIN,
            WaitResult::OutOfMemory => return -errno::ENOMEM,
        }
    }
}

pub(crate) fn sys_accept(fd: usize, address: usize, length: usize) -> isize {
    sys_accept4(fd, address, length, 0)
}

pub(crate) fn sys_getsockname(fd: usize, address: usize, length: usize) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .address()
        .map_err(socket_error)
        .and_then(|value| write_address(&current_task().unwrap(), value, address, length))
        .map_or_else(|error| error, |()| 0)
}

pub(crate) fn sys_getpeername(fd: usize, address: usize, length: usize) -> isize {
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket
        .peer_address()
        .map_err(socket_error)
        .and_then(|value| write_address(&current_task().unwrap(), value, address, length))
        .map_or_else(|error| error, |()| 0)
}

pub(crate) fn sys_shutdown(fd: usize, how: usize) -> isize {
    if how > 2 {
        return -errno::EINVAL;
    }
    let (_ofd, socket) = match socket_ofd(fd) {
        Ok(value) => value,
        Err(error) => return error,
    };
    socket.shutdown(how).map_or_else(socket_error, |()| 0)
}

fn encode_inet_sockaddr(request: &mut [u8; IFREQ_SIZE], address: core::net::Ipv4Addr) {
    request[16..32].fill(0);
    request[16..18].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    request[20..24].copy_from_slice(&address.octets());
}

fn decode_inet_sockaddr(request: &[u8; IFREQ_SIZE]) -> Result<core::net::Ipv4Addr, isize> {
    if u16::from_ne_bytes(request[16..18].try_into().unwrap()) as usize != AF_INET {
        return Err(-errno::EINVAL);
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
