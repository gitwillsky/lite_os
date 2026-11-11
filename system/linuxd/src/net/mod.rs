use alloc::sync::Arc;

use crate::ipc::PipeEnd;
use crate::ipc::ReceiveBuffer;

mod inet;
mod kobject;
mod message_limits;
mod observation;
mod preparation;
mod rtnetlink;
pub(crate) use preparation::{PreparedSocket, SocketRequest};
mod options;
mod timeouts;
use timeouts::SocketTimeouts;
pub(crate) use timeouts::{SocketIoDirection, SocketTimeout};
mod capacity;
mod control_memory;
mod core_buffer_limit;
mod core_buffers;
mod net_namespace;
pub(crate) use control_memory::{ControlMemoryCharge, SocketMemory};
pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
mod credentials;
pub(crate) use core_buffer_limit::CoreBufferLimit;
pub(crate) use credentials::{UnixCredentials, UnixSendCredentials};
mod netstack;
mod packet;
mod retirement;
mod rights;
mod unix;

pub(crate) use capacity::{
    SocketCapacityWait, SocketConnectError, SocketSendError, SocketWaitGuard,
};
use inet::{InetProtocol, InetSocket};
use kobject::KobjectSocket;
pub(crate) use kobject::{publish_drm_hotplug, publish_input};
use packet::PacketSocket;
use rtnetlink::RouteSocket;
use unix::UnixSocket;
pub(crate) use unix::{
    SCM_MAX_FD, UnixAddress, UnixFileLimit, UnixNode, UnixPassedFile, UnixPathIdentity,
    UnixReceivedRights, UnixRights,
};

pub(crate) use inet::{
    address_snapshot, configure_address, configure_broadcast, configure_mtu, configure_netmask,
    configure_route, configure_up, interface_index, interface_snapshot, interface_snapshots,
    neighbor_snapshot, network_snapshot, route_lookup, route_snapshot, route_snapshots,
};
pub(crate) use netstack::init;
pub(crate) use network_protocol::{
    InetAddress, InterfaceSnapshot, InternetOption, InternetPacketInfo, InternetSendPacketInfo,
    Ipv4RouteError, Ipv4RouteRequest, PacketAddress, PacketChecksum, PacketInformation,
    RouteChange, RouteKind, RouteMetrics, RoutingSnapshot, SocketError, SocketPollState,
};

/// 返回正 errno。syscall 返回负值，netlink 把负值写入 NLMSG_ERROR。
pub(crate) fn socket_errno(error: SocketError) -> i32 {
    use linux_abi::errno;
    (match error {
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
        SocketError::AddressFamilyNotSupported => errno::EAFNOSUPPORT,
        SocketError::NoEntry => errno::ENOENT,
        SocketError::ProtocolError => errno::EPROTO,
        SocketError::ProtocolOptionUnavailable => errno::ENOPROTOOPT,
        SocketError::HostDown => errno::EHOSTDOWN,
        SocketError::NetworkIsolated => errno::ENONET,
    }) as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketDomain {
    Unix,
    Inet,
    Inet6,
    Packet,
    Netlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SocketType {
    Stream,
    Datagram,
    SeqPacket,
    Raw,
}

/// Linux `sockaddr_nl` 中与 KOBJECT_UEVENT 有关的语义字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NetlinkAddress {
    pub(crate) port_id: u32,
    pub(crate) groups: u32,
}

pub(crate) struct ReceivedMessage {
    pub(crate) count: usize,
    pub(crate) full_length: usize,
    pub(crate) source: Option<SocketAddress>,
    pub(crate) internet_error: Option<network_protocol::InternetError>,
    pub(crate) internet_packet_info: Option<InternetPacketInfo>,
    pub(crate) packet_information: Option<PacketInformation>,
    pub(crate) rights: Option<UnixReceivedRights>,
    pub(crate) credentials: Option<UnixCredentials>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SocketAddress {
    Unix(UnixAddress),
    Inet(InetAddress),
    Packet(PacketAddress),
    Netlink(NetlinkAddress),
}

/// 已由上层 pathname/VFS seam 授权的发送目的；下层不重新解释 pathname 为 abstract 名字。
pub(crate) enum SocketSendDestination {
    Address(SocketAddress),
    UnixPath(UnixPathIdentity),
}

enum SocketBackend {
    Unix(Arc<UnixSocket>),
    Inet(Arc<InetSocket>),
    Packet(Arc<PacketSocket>),
    Kobject(Arc<KobjectSocket>),
    Route(Arc<RouteSocket>),
}

/// OFD 唯一 socket backend facade；AF_UNIX/AF_INET adapter 不穿透 fs seam。
pub(crate) struct Socket {
    domain: SocketDomain,
    socket_type: SocketType,
    backend: SocketBackend,
    memory: Arc<SocketMemory>,
    // OWNER: 超时 policy 属于实际 socket/OFD；不随 dup/fork 复制，不与 backend 平行存储。
    timeouts: Arc<SocketTimeouts>,
}

/// 上层仅为实际协议准备必需的传输资源；packet 不构造未使用的 byte Pipes。
pub(crate) enum UnixConnectionData {
    Stream {
        first_to_second: (Arc<PipeEnd>, Arc<PipeEnd>),
        second_to_first: (Arc<PipeEnd>, Arc<PipeEnd>),
    },
    Packet,
}
/// AF_UNIX oriented connect 的实际 child notification 与 typed transport。
pub(crate) struct UnixConnectResources {
    pub(crate) server_notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    pub(crate) data: UnixConnectionData,
}

mod wait_source;
pub(crate) use wait_source::SocketWaitSource;

// 最大三项：stream 的实际 read/write transport，加 endpoint error/lifecycle notification。
pub(crate) type SocketWaitSources = [Option<SocketWaitSource>; 3];

impl Socket {
    pub(crate) fn domain(&self) -> SocketDomain {
        self.domain
    }

    pub(crate) fn socket_type(&self) -> SocketType {
        self.socket_type
    }

    /// 投影实际 endpoint 的监听状态，供 SO_ACCEPTCONN 查询；无状态复制。
    /// 非面向连接的协议返回 false，IPv4 owner 查询失败时保留原错误。
    pub(crate) fn is_listening(&self) -> Result<bool, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.is_listening()),
            SocketBackend::Inet(socket) => socket.is_listening(),
            SocketBackend::Packet(_) | SocketBackend::Kobject(_) | SocketBackend::Route(_) => {
                Ok(false)
            }
        }
    }

    pub(crate) fn bind(
        self: &Arc<Self>,
        address: SocketAddress,
        caller_tgid: u32,
    ) -> Result<(), SocketError> {
        match (&self.backend, address) {
            (SocketBackend::Unix(socket), SocketAddress::Unix(address)) => socket.bind(address),
            (SocketBackend::Inet(socket), SocketAddress::Inet(address)) => socket.bind(address),
            (SocketBackend::Packet(socket), SocketAddress::Packet(address)) => socket.bind(address),
            (SocketBackend::Kobject(socket), SocketAddress::Netlink(address)) => {
                socket.bind(address, caller_tgid)
            }
            (SocketBackend::Route(socket), SocketAddress::Netlink(address)) => {
                socket.bind(address, caller_tgid)
            }
            _ => Err(SocketError::Invalid),
        }
    }

    /// 将 AF_UNIX endpoint 绑定到已创建的 VFS socket inode。
    ///
    /// # Parameters
    ///
    /// - `address`: canonical pathname sockaddr value。
    /// - `identity`: VFS 返回的稳定 inode identity。
    ///
    /// # Returns
    ///
    /// endpoint namespace publication 成功。
    ///
    /// # Errors
    ///
    /// 非 AF_UNIX、重复 bind、collision 或 OOM 返回明确错误。
    pub(crate) fn bind_unix_path(
        self: &Arc<Self>,
        address: UnixAddress,
        identity: UnixPathIdentity,
        owner: Arc<dyn Send + Sync>,
    ) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.bind_path(address, identity, owner),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn listen(
        &self,
        backlog: usize,
        credentials: Option<UnixCredentials>,
    ) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => {
                socket.listen(backlog, credentials.ok_or(SocketError::Invalid)?)
            }
            SocketBackend::Inet(socket) => socket.listen(backlog),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::Kobject(_) | SocketBackend::Route(_) => {
                Err(SocketError::OperationNotSupported)
            }
        }
    }

    /// Linux AF_UNSPEC 取消当前 peer/连接，保留对应 domain 的本地绑定。
    pub(crate) fn disconnect(&self) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) if self.socket_type == SocketType::Datagram => {
                socket.disconnect_datagram()
            }
            SocketBackend::Inet(socket) => socket.disconnect(),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn connect<F>(
        self: &Arc<Self>,
        address: SocketAddress,
        unix_credentials: Option<UnixCredentials>,
        unix_path: Option<UnixPathIdentity>,
        unix_resources: F,
    ) -> Result<(), SocketConnectError>
    where
        F: FnOnce() -> Result<UnixConnectResources, SocketError>,
    {
        match (&self.backend, address) {
            (SocketBackend::Inet(socket), SocketAddress::Inet(address)) => {
                socket.connect(address).map_err(Into::into)
            }
            (SocketBackend::Unix(client), SocketAddress::Unix(address)) => {
                client.autobind_if_credentials()?;
                let listener = if let Some(identity) = unix_path {
                    UnixSocket::lookup_path(identity)?
                } else {
                    UnixSocket::lookup(&address, self.socket_type, &self.net_namespace())?
                };
                if self.socket_type == SocketType::Datagram {
                    return client.connect_datagram(&listener).map_err(|error| {
                        if error == SocketError::ConnectionRefused && listener.is_closed() {
                            SocketConnectError::DestinationChanged
                        } else {
                            error.into()
                        }
                    });
                }
                match UnixSocket::connect_oriented(
                    client,
                    &listener,
                    unix_credentials.ok_or(SocketError::Invalid)?,
                    unix_resources,
                ) {
                    Ok(()) => Ok(()),
                    Err(SocketError::Again) => Err(SocketConnectError::BacklogFull(
                        SocketCapacityWait::new(listener, capacity::CapacityKind::Backlog),
                    )),
                    Err(SocketError::ConnectionRefused) if listener.is_closed() => {
                        Err(SocketConnectError::DestinationChanged)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            (SocketBackend::Kobject(_) | SocketBackend::Route(_), _) => {
                Err(SocketError::OperationNotSupported.into())
            }
            _ => Err(SocketError::Invalid.into()),
        }
    }

    pub(crate) fn pair(
        first: &Arc<Self>,
        second: &Arc<Self>,
        data: UnixConnectionData,
        credentials: UnixCredentials,
    ) -> Result<(), SocketError> {
        let (SocketBackend::Unix(first), SocketBackend::Unix(second)) =
            (&first.backend, &second.backend)
        else {
            return Err(SocketError::OperationNotSupported);
        };
        UnixSocket::pair(first, second, data, credentials)
    }

    /// 解析可能异步完成的 domain connect 结果。
    ///
    /// # Returns
    ///
    /// 已完成连接返回 unit。
    ///
    /// # Errors
    ///
    /// 返回进行中、拒绝或未连接错误。
    pub(crate) fn connection_result(&self) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.connection_result(),
            SocketBackend::Unix(_) => Ok(()),
            SocketBackend::Packet(_) | SocketBackend::Kobject(_) | SocketBackend::Route(_) => {
                Err(SocketError::OperationNotSupported)
            }
        }
    }

    /// 原子读取并清除 domain pending error。
    ///
    /// # Returns
    ///
    /// pending error；没有时为 None。
    pub(crate) fn take_error(&self) -> Option<SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.take_error(),
            SocketBackend::Unix(socket) => socket.take_error(),
            SocketBackend::Packet(socket) => socket.take_error(),
            SocketBackend::Kobject(socket) => socket.take_error(),
            SocketBackend::Route(socket) => socket.take_error(),
        }
    }

    pub(crate) fn read(
        &self,
        output: &mut ReceiveBuffer<'_>,
        nonblocking: bool,
    ) -> Result<usize, SocketError> {
        // Native sock_read_iter 对空 iterator 返回零，所有 protocol queues 均不消费。
        if output.remaining() == 0 {
            return Ok(0);
        }
        self.receive_request(output, false, unix::ReceiveMode::Read { nonblocking })
            .map(|message| message.count)
    }

    pub(crate) fn receive_message(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
        receive_rights: bool,
        nonblocking: bool,
    ) -> Result<ReceivedMessage, SocketError> {
        self.receive_request(
            output,
            peek,
            unix::ReceiveMode::Message {
                rights: receive_rights,
                nonblocking,
            },
        )
    }

    /// Consume one error queue record without waiting.
    pub(crate) fn receive_error_message(
        &self,
        output: &mut ReceiveBuffer<'_>,
    ) -> Result<ReceivedMessage, SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.receive_error(output),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn set_receive_error(&self, ipv6: bool, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_receive_error(ipv6, enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    fn receive_request(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
        mode: unix::ReceiveMode,
    ) -> Result<ReceivedMessage, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.receive(output, mode, peek).map(
                |(count, full_length, source, rights, credentials)| ReceivedMessage {
                    count,
                    full_length,
                    source: source.map(SocketAddress::Unix),
                    internet_error: None,
                    internet_packet_info: None,
                    packet_information: None,
                    rights,
                    credentials,
                },
            ),
            SocketBackend::Inet(socket) => socket.receive(output, peek).map(
                |(count, full_length, source, internet_packet_info)| ReceivedMessage {
                    count,
                    full_length,
                    source: source.map(SocketAddress::Inet),
                    internet_error: None,
                    internet_packet_info,
                    packet_information: None,
                    rights: None,
                    credentials: None,
                },
            ),
            SocketBackend::Packet(socket) => {
                socket
                    .receive(output, peek)
                    .map(
                        |(count, full_length, source, information)| ReceivedMessage {
                            count,
                            full_length,
                            source: Some(SocketAddress::Packet(source)),
                            internet_error: None,
                            internet_packet_info: None,
                            packet_information: Some(information),
                            rights: None,
                            credentials: None,
                        },
                    )
            }
            SocketBackend::Kobject(socket) => socket.receive(output, peek),
            SocketBackend::Route(socket) => socket.receive(output, peek),
        }
    }

    pub(crate) fn validate_send_length(&self, length: usize) -> Result<(), SocketError> {
        if let SocketBackend::Unix(socket) = &self.backend
            && matches!(
                self.socket_type,
                SocketType::Datagram | SocketType::SeqPacket
            )
        {
            return socket.validate_packet_length(length);
        }
        message_limits::validate_send_length(
            message_limits::protocol(self.domain, self.socket_type),
            length,
        )
        .map_err(|()| SocketError::MessageTooLarge)
    }

    /// 为 stream send 选择固定上限 staging；atomic protocol 返回 None。
    pub(crate) fn stream_send_staging_capacity(
        &self,
        requested: usize,
        stream_max: usize,
    ) -> Option<usize> {
        message_limits::stream_send_capacity(
            message_limits::protocol(self.domain, self.socket_type),
            requested,
            stream_max,
        )
    }

    /// 准备一次 receive 的唯一 byte sink；packet 按实际队列准备，stream/IP 使用协议上限。
    /// 用户最大容量保留在 ReceiveBuffer 中，空 packet queue 不导致 request-sized 分配。
    pub(crate) fn prepare_receive(
        &self,
        requested: usize,
        stream_max: usize,
    ) -> Result<ReceiveBuffer<'static>, SocketError> {
        if matches!(&self.backend, SocketBackend::Unix(_)) && self.socket_type != SocketType::Stream
        {
            return Ok(ReceiveBuffer::deferred(requested));
        }
        let capacity = message_limits::receive_capacity(
            message_limits::protocol(self.domain, self.socket_type),
            requested,
            stream_max,
        );
        ReceiveBuffer::try_new(capacity).map_err(|()| SocketError::NoMemory)
    }

    /// Native SeqPacket error/state 检查先于 SCM 语义解析，但晚于完整 address/control copyin。
    pub(crate) fn check_seqpacket_send(&self) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) if self.socket_type == SocketType::SeqPacket => {
                socket.check_seqpacket_send()
            }
            _ => Ok(()),
        }
    }
    /// Native Unix stream name/own-write permission 在 payload copy 前检查；零 bytes 不探测 peer SHUT_RD。
    pub(crate) fn check_stream_send(&self, named: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) if self.socket_type == SocketType::Stream => {
                socket.check_stream_send(named)
            }
            _ => Ok(()),
        }
    }

    /// 发送 payload，并只允许 AF_UNIX backend 原子提交 SCM_RIGHTS。
    ///
    /// # Parameters
    ///
    /// - `input`: 本次 byte payload。
    /// - `target`: 可选显式 socket address。
    /// - `rights`: 尚未提交的 passed-file capability 集合。
    /// - `net_admin`: 发送者在该 NET 的 owning UserNS 里是否有 CAP_NET_ADMIN；只有 rtnetlink 修改请求用它。
    ///
    /// # Returns
    ///
    /// 实际 byte count；失败时 rights 不被消费。
    pub(crate) fn send_to_with_ancillary(
        &self,
        input: &[u8],
        target: Option<SocketSendDestination>,
        rights: &mut Option<UnixRights>,
        credentials: &UnixSendCredentials,
        packet_info: impl ExactSizeIterator<Item = InternetSendPacketInfo>,
        net_admin: bool,
    ) -> Result<usize, SocketSendError> {
        self.validate_send_length(input.len())
            .map_err(SocketSendError::from)?;
        match (&self.backend, target) {
            (SocketBackend::Unix(socket), _) if self.socket_type == SocketType::SeqPacket => {
                socket.send_to_with_ancillary(input, None, rights, credentials)
            }
            (SocketBackend::Unix(socket), Some(SocketSendDestination::UnixPath(identity))) => {
                let target = UnixSocket::lookup_path(identity).map_err(SocketSendError::from)?;
                socket.send_to_with_ancillary(input, Some(&target), rights, credentials)
            }
            (
                SocketBackend::Unix(socket),
                Some(SocketSendDestination::Address(SocketAddress::Unix(address))),
            ) => {
                let target = UnixSocket::lookup(&address, self.socket_type, &self.net_namespace())
                    .map_err(SocketSendError::from)?;
                socket.send_to_with_ancillary(input, Some(&target), rights, credentials)
            }
            (SocketBackend::Unix(socket), None) => {
                socket.send_to_with_ancillary(input, None, rights, credentials)
            }
            (_, _) if rights.is_some() => Err(SocketError::OperationNotSupported.into()),
            (
                SocketBackend::Inet(socket),
                Some(SocketSendDestination::Address(SocketAddress::Inet(address))),
            ) => socket
                .send_to(input, Some(address), packet_info)
                .map_err(Into::into),
            (SocketBackend::Inet(socket), None) => {
                socket.send_to(input, None, packet_info).map_err(Into::into)
            }
            (
                SocketBackend::Packet(socket),
                Some(SocketSendDestination::Address(SocketAddress::Packet(address))),
            ) => socket.send_to(input, Some(address)).map_err(Into::into),
            (SocketBackend::Packet(socket), None) => {
                socket.send_to(input, None).map_err(Into::into)
            }
            (
                SocketBackend::Route(socket),
                Some(SocketSendDestination::Address(SocketAddress::Netlink(address))),
            ) => socket
                .send(input, Some(address), net_admin)
                .map_err(Into::into),
            (SocketBackend::Route(socket), None) => {
                socket.send(input, None, net_admin).map_err(Into::into)
            }
            (SocketBackend::Kobject(_), _) => Err(SocketError::OperationNotSupported.into()),
            _ => Err(SocketError::Invalid.into()),
        }
    }

    /// 首次发送前为 `NETLINK_ROUTE` 自动绑定 port（Linux `netlink_autobind`）；其他 backend 无操作。
    ///
    /// # Parameters
    ///
    /// - `caller_tgid`: 调用者的可见 TGID，优先作为 port。
    pub(crate) fn autobind_netlink(&self, caller_tgid: u32) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Route(socket) => socket.autobind(caller_tgid),
            _ => Ok(()),
        }
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.shutdown(how),
            SocketBackend::Inet(socket) => socket.shutdown(how),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::Kobject(_) | SocketBackend::Route(_) => {
                Err(SocketError::OperationNotSupported)
            }
        }
    }
}

/// 运行 AF_UNIX cycle collector；worker/deadline reservation 由 composition root 装配。
pub(crate) fn run_unix_rights_collector(
    clock: impl Fn() -> u64,
    sleep_until: impl FnMut(u64),
    gate: crate::sync::TaskMutexWaitPreparation,
) -> ! {
    unix::run_rights_collector(clock, sleep_until, gate)
}
