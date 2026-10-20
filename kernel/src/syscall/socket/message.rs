use alloc::{sync::Arc, vec::Vec};

use super::{
    MSG_DONTWAIT, MSG_NOSIGNAL, MSG_PEEK, MSG_TRUNC, O_NONBLOCK, SocketAddress, SocketError,
    TaskControlBlock, WaitResult, errno, read_address, socket_error, socket_ofd, wait_for_ofd,
    write_address,
};
use crate::{
    file::OpenFileDescription,
    socket::{Socket, SocketIoDirection, SocketSendError},
    syscall::{
        poll::wait_for_socket_capacity,
        user_iovec::{
            BufferError, ImportError, UserInputStaging, UserIoCursor, UserIoVec,
            bounded_staging_capacity, import_iovecs, project_total_length, validate_user_buffers,
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
    let name = usize::from_ne_bytes(bytes[..8].try_into().unwrap());
    let name_length = if name == 0 {
        0
    } else {
        i32::from_ne_bytes(bytes[8..12].try_into().unwrap())
    };
    if name_length < 0 {
        return Err(-errno::EINVAL);
    }
    Ok(MessageHeader {
        name,
        name_length: (name_length as usize).min(128),
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
            ImportError::TooMany => -errno::EMSGSIZE,
            ImportError::NullArray => -errno::EINVAL,
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

fn send_error(
    task: &TaskControlBlock,
    socket: &Socket,
    flags: usize,
    error: SocketError,
    completed: usize,
) -> isize {
    // Linux 仅 TCP/unix stream 的 out_pipe 生成 SIGPIPE；UDP/unix packet 的分配 EPIPE 不发信号。
    if error == SocketError::BrokenPipe
        && completed == 0
        && flags & MSG_NOSIGNAL == 0
        && socket.socket_type() == crate::socket::SocketType::Stream
    {
        send_thread_signal(task.tgid(), task.tid(), syscall_abi::signal::SIGPIPE)
            .expect("current socket sender must remain live");
    }
    if completed == 0 {
        socket_error(error)
    } else {
        completed as isize
    }
}

/// 已 copyin 的目的输入；Unix stream 只关心非空 name 是否存在，不解析其 family。
enum SendTarget {
    Address(SocketAddress),
    StreamName,
}

type ResolvedSendDestination = (
    Option<crate::socket::SocketSendDestination>,
    Option<Arc<dyn crate::fs::Inode>>,
);

struct SendContext<'a> {
    task: &'a TaskControlBlock,
    ofd: &'a Arc<OpenFileDescription>,
    socket: &'a Arc<Socket>,
    target: Option<SendTarget>,
    flags: usize,
    nonblocking: bool,
    deadline: Option<u64>,
    credentials: crate::socket::UnixSendCredentials,
    packet_info: Option<crate::socket::InternetSendPacketInfo>,
}

/// Native move_addr_to_kernel 先校验/copy 通用 sockaddr；SeqPacket/TCP 随后忽略目的地址。
fn read_target(
    socket: &Socket,
    task: &TaskControlBlock,
    pointer: usize,
    length: usize,
) -> Result<Option<SendTarget>, isize> {
    if pointer == 0 || length == 0 {
        return Ok(None);
    }
    if (socket.domain() == crate::socket::SocketDomain::Unix
        && matches!(
            socket.socket_type(),
            crate::socket::SocketType::Stream | crate::socket::SocketType::SeqPacket
        ))
        || (socket.domain() == crate::socket::SocketDomain::Inet
            && socket.socket_type() == crate::socket::SocketType::Stream)
    {
        if length > 128 {
            return Err(-errno::EINVAL);
        }
        let mut ignored = [0u8; 128];
        if length != 0 {
            task.copy_from_user(pointer, &mut ignored[..length])
                .map_err(|_| -errno::EFAULT)?;
        }
        return Ok((socket.domain() == crate::socket::SocketDomain::Unix
            && socket.socket_type() == crate::socket::SocketType::Stream)
            .then_some(SendTarget::StreamName));
    }
    read_address(task, pointer, length, false).map(|address| Some(SendTarget::Address(address)))
}

impl SendContext<'_> {
    /// 发送者在 socket 所属 NET 的 owning UserNS 里是否有 CAP_NET_ADMIN（Linux `netlink_net_capable`）。
    /// 只有 netlink socket 需要，其他 domain 不查询 capability。
    fn net_admin(&self) -> bool {
        self.socket.domain() == crate::socket::SocketDomain::Netlink
            && self.task.has_namespace_capability(
                &self.socket.net_namespace().user_namespace(),
                syscall_abi::capability::Capability::NET_ADMIN,
            )
    }

    fn nonblocking(&self) -> bool {
        self.nonblocking
    }
    // 真实 VFS inode pin 活到一次 backend lookup/commit；capacity 重试重新解析实际 caller namespace。
    fn destination(&self) -> Result<ResolvedSendDestination, isize> {
        if let Some(SendTarget::Address(SocketAddress::Unix(address))) = &self.target
            && !address.is_abstract()
        {
            let (inode, identity) = super::unix_path::resolve(self.task, address, true)?;
            return Ok((
                Some(crate::socket::SocketSendDestination::UnixPath(identity)),
                Some(inode),
            ));
        }
        Ok((
            self.target.as_ref().and_then(|target| match target {
                SendTarget::Address(address) => Some(
                    crate::socket::SocketSendDestination::Address(address.clone()),
                ),
                SendTarget::StreamName => None,
            }),
            None,
        ))
    }
}

fn send_one_message(
    context: &SendContext<'_>,
    bytes: &[u8],
    rights: &mut Option<crate::socket::UnixRights>,
) -> isize {
    // Linux 在首次 sendmsg 里 autobind netlink socket；回复要有可投递的 port。
    if let Err(error) = context
        .socket
        .autobind_netlink(current_task().unwrap().visible_tgid())
    {
        return socket_error(error);
    }
    loop {
        let (target, _inode) = match context.destination() {
            Ok(target) => target,
            Err(error) => return error,
        };
        match context.socket.send_to_with_ancillary(
            bytes,
            target,
            rights,
            &context.credentials,
            context.packet_info,
            context.net_admin(),
        ) {
            Ok(count) => return count as isize,
            Err(SocketSendError::DestinationChanged) => continue,
            Err(SocketSendError::WouldBlock | SocketSendError::Capacity(_))
                if context.nonblocking() =>
            {
                return -errno::EAGAIN;
            }
            Err(SocketSendError::WouldBlock) => {
                match wait_for_ofd(context.ofd, 4, context.deadline) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return super::interrupted_wait(context.deadline),
                    WaitResult::TimedOut => return -errno::EAGAIN,
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                }
            }
            Err(SocketSendError::Capacity(blocker)) => {
                match wait_for_socket_capacity(&blocker, context.deadline) {
                    WaitResult::Woken => {}
                    WaitResult::Interrupted => return super::interrupted_wait(context.deadline),
                    WaitResult::TimedOut => return -errno::EAGAIN,
                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                }
            }
            Err(SocketSendError::Error(error)) => {
                return send_error(context.task, context.socket, context.flags, error, 0);
            }
        }
    }
}

/// 阻塞 stream 继续等待未发送 suffix；signal/timeout/OOM 只有没有进度时才暴露错误。
fn stream_send_wait(
    context: &SendContext<'_>,
    result: WaitResult,
    completed: usize,
) -> Option<isize> {
    let error = match result {
        WaitResult::Woken => return None,
        WaitResult::Interrupted => super::interrupted_wait(context.deadline),
        WaitResult::TimedOut => -errno::EAGAIN,
        WaitResult::OutOfMemory => -errno::ENOMEM,
    };
    Some(if completed == 0 {
        error
    } else {
        completed as isize
    })
}

fn send_stream_message(
    context: &SendContext<'_>,
    vectors: &[UserIoVec],
    total_length: usize,
    mut rights: Option<crate::socket::UnixRights>,
) -> isize {
    if total_length == 0 {
        // Native stream 零长度 send 不发布 skb/rights，仍已在入口完整校验 cmsg。
        return send_one_message(context, &[], &mut rights);
    }
    let staging_capacity = context
        .socket
        .stream_send_staging_capacity(total_length, STREAM_STAGING_BYTES)
        .expect("stream send path must retain facade-selected staging policy");
    let mut staging = match UserInputStaging::try_new(staging_capacity) {
        Ok(bytes) => bytes,
        Err(()) => return -errno::ENOMEM,
    };
    let mut cursor = UserIoCursor::new(vectors);
    while cursor.completed() < total_length {
        let capacity =
            bounded_staging_capacity(total_length - cursor.completed(), staging.capacity());
        let staged = cursor.stage_from_user_into(context.task, &mut staging, capacity);
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
            let (target, _inode) = match context.destination() {
                Ok(target) => target,
                Err(error) => {
                    return if cursor.completed() == 0 {
                        error
                    } else {
                        cursor.completed() as isize
                    };
                }
            };
            match context.socket.send_to_with_ancillary(
                staging.initialized(),
                target,
                &mut rights,
                &context.credentials,
                context.packet_info,
                context.net_admin(),
            ) {
                Ok(sent) => {
                    assert!(sent <= staged.count, "socket consumed beyond staged prefix");
                    cursor.advance(sent);
                    if sent == 0 {
                        return cursor.completed() as isize;
                    }
                    break;
                }
                Err(SocketSendError::DestinationChanged) => continue,
                Err(SocketSendError::WouldBlock | SocketSendError::Capacity(_))
                    if context.nonblocking() =>
                {
                    return if cursor.completed() == 0 {
                        -errno::EAGAIN
                    } else {
                        cursor.completed() as isize
                    };
                }
                Err(SocketSendError::WouldBlock) => {
                    if let Some(result) = stream_send_wait(
                        context,
                        wait_for_ofd(context.ofd, 4, context.deadline),
                        cursor.completed(),
                    ) {
                        return result;
                    }
                }
                Err(SocketSendError::Capacity(blocker)) => {
                    if let Some(result) = stream_send_wait(
                        context,
                        wait_for_socket_capacity(&blocker, context.deadline),
                        cursor.completed(),
                    ) {
                        return result;
                    }
                }
                Err(SocketSendError::Error(error)) => {
                    return send_error(
                        context.task,
                        context.socket,
                        context.flags,
                        error,
                        cursor.completed(),
                    );
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
    if let Err(error) = context
        .socket
        .check_stream_send(matches!(context.target, Some(SendTarget::StreamName)))
    {
        return send_error(context.task, context.socket, context.flags, error, 0);
    }
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
    let mut bytes = match UserInputStaging::try_new(total_length) {
        Ok(bytes) => bytes,
        Err(()) => return -errno::ENOMEM,
    };
    let mut cursor = UserIoCursor::new(vectors);
    if cursor
        .copy_from_user_into(context.task, &mut bytes, total_length)
        .is_err()
        || cursor.completed() != total_length
    {
        return -errno::EFAULT;
    }
    let mut rights = rights;
    send_one_message(&context, bytes.initialized(), &mut rights)
}

/// scalar/writev 的 socket 分支只装配无 name/control 的原生 request，和 sendmsg 共用 byte/progress/wait owner。
pub(in crate::syscall) fn write_vectors(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    socket: &Arc<Socket>,
    vectors: &[UserIoVec],
    total_length: usize,
    nonblocking: bool,
    flags: usize,
) -> isize {
    if let Err(error) = socket.check_seqpacket_send() {
        return socket_error(error);
    }
    send_message(
        SendContext {
            task,
            ofd,
            socket,
            target: None,
            flags,
            nonblocking: nonblocking || ofd.status_flags() & O_NONBLOCK != 0,
            deadline: socket
                .timeout(SocketIoDirection::Send)
                .deadline(crate::timer::get_time_ns()),
            credentials: super::credentials::sender(task),
            packet_info: None,
        },
        vectors,
        total_length,
        None,
    )
}

/// Linux sendmsg scatter/gather ABI，复用唯一 socket send path。
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
    let target = match read_target(&socket, &task, header.name, header.name_length) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let snapshot = match super::control_list::ControlSnapshot::import(
        &task,
        &socket,
        header.control,
        header.control_length,
    ) {
        Ok(snapshot) => snapshot,
        Err(super::control_list::ImportError::NoBuffer) => return -errno::ENOBUFS,
        Err(super::control_list::ImportError::CopyFault) => return -errno::EFAULT,
    };
    if let Err(error) = socket.check_seqpacket_send() {
        return socket_error(error);
    }
    let ancillary = match super::control::parse_send(&task, &socket, &snapshot) {
        Ok(ancillary) => ancillary,
        Err(error) => return error,
    };
    let result = send_message(
        SendContext {
            task: &task,
            credentials: ancillary.credentials,
            packet_info: ancillary.packet_info,
            ofd: &ofd,
            socket: &socket,
            target,
            flags,
            nonblocking: flags & MSG_DONTWAIT != 0 || ofd.status_flags() & O_NONBLOCK != 0,
            deadline: socket
                .timeout(SocketIoDirection::Send)
                .deadline(crate::timer::get_time_ns()),
        },
        &iovecs,
        total_length,
        ancillary.rights,
    );
    // 原生 socket.c 在协议 send 返回后才 kfree control；阻塞期间与所有 failure 保持同一费用。
    drop(snapshot);
    result
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
    let target = match read_target(&socket, &task, address, address_length) {
        Ok(target) => target,
        Err(error) => return error,
    };
    if let Err(error) = socket.check_seqpacket_send() {
        return socket_error(error);
    }
    send_message(
        SendContext {
            task: &task,
            credentials: super::credentials::sender(&task),
            packet_info: None,
            ofd: &ofd,
            socket: &socket,
            target,
            flags,
            nonblocking: flags & MSG_DONTWAIT != 0 || ofd.status_flags() & O_NONBLOCK != 0,
            deadline: socket
                .timeout(SocketIoDirection::Send)
                .deadline(crate::timer::get_time_ns()),
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
    let mut output = match socket.prepare_receive(length, STREAM_STAGING_BYTES) {
        Ok(output) => output,
        Err(error) => return socket_error(error),
    };
    let deadline = socket
        .timeout(SocketIoDirection::Receive)
        .deadline(crate::timer::get_time_ns());
    let nonblocking = flags & MSG_DONTWAIT != 0 || ofd.status_flags() & O_NONBLOCK != 0;
    loop {
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0, false, nonblocking) {
            Ok(received) => {
                let mut cursor = UserIoCursor::new(&vectors);
                if cursor.copy_to_user(&task, output.initialized()).is_err() {
                    return -errno::EFAULT;
                }
                if let Err(error) = write_address(&task, received.source, address, address_length) {
                    return error;
                }
                return if flags & MSG_TRUNC != 0 {
                    received.full_length as isize
                } else {
                    received.count as isize
                };
            }
            Err(SocketError::Again) if nonblocking => {
                return -errno::EAGAIN;
            }
            Err(SocketError::Again) => match wait_for_ofd(&ofd, 1, deadline) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return super::interrupted_wait(deadline),
                WaitResult::TimedOut => return -errno::EAGAIN,
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
            Err(error) => return socket_error(error),
        }
    }
}

/// Linux recvmsg scatter/gather、MSG_PEEK 与 IPv4 PKTINFO ancillary ABI。
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
    let mut output = match socket.prepare_receive(total_length, STREAM_STAGING_BYTES) {
        Ok(output) => output,
        Err(error) => return socket_error(error),
    };
    let nonblocking = flags & MSG_DONTWAIT != 0 || ofd.status_flags() & O_NONBLOCK != 0;
    let deadline = socket
        .timeout(SocketIoDirection::Receive)
        .deadline(crate::timer::get_time_ns());
    loop {
        match socket.receive_message(&mut output, flags & MSG_PEEK != 0, true, nonblocking) {
            Ok(received) => {
                let mut cursor = UserIoCursor::new(&iovecs);
                if cursor.copy_to_user(&task, output.initialized()).is_err()
                    || cursor.completed() != received.count
                {
                    return -errno::EFAULT;
                }
                let target = super::control::ReceiveTarget {
                    task: &task,
                    message,
                    name: header.name,
                    control: (header.control, header.control_length),
                };
                let content = super::control::ReceiveContent {
                    source: received.source,
                    internet_packet_info: received.internet_packet_info,
                    packet_info: socket.ipv4_packet_info(),
                    rights: received.rights,
                    credentials: received.credentials,
                    pass_credentials: socket.pass_credentials(),
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
            Err(SocketError::Again) => match wait_for_ofd(&ofd, 1, deadline) {
                WaitResult::Woken => {}
                WaitResult::Interrupted => return super::interrupted_wait(deadline),
                WaitResult::TimedOut => return -errno::EAGAIN,
                WaitResult::OutOfMemory => return -errno::ENOMEM,
            },
            Err(error) => return socket_error(error),
        }
    }
}
