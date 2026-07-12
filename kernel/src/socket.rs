use alloc::{sync::Arc, vec::Vec};
use core::net::Ipv4Addr;

use crate::ipc::{Pipe, PipeDirection, PipeEnd};

#[path = "socket/inet.rs"]
mod inet;
#[path = "socket/packet.rs"]
mod packet;
#[path = "socket/unix.rs"]
mod unix;

use inet::InetSocket;
use packet::PacketSocket;
pub(crate) use unix::UnixAddress;
use unix::UnixSocket;

pub(crate) use inet::{
    configure_address, configure_gateway, configure_netmask, configure_up, dispatch_network_work,
    interface_snapshot, network_snapshot, network_work_due,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketDomain {
    Unix,
    Inet,
    Packet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketType {
    Stream,
    Datagram,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InetAddress {
    pub(crate) address: Ipv4Addr,
    pub(crate) port: u16,
}

/// @description Linux `sockaddr_ll` 的 domain value，不暴露 userspace padding。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PacketAddress {
    pub(crate) protocol: u16,
    pub(crate) interface_index: i32,
    pub(crate) hardware_type: u16,
    pub(crate) packet_type: u8,
    pub(crate) address_length: u8,
    pub(crate) address: [u8; 8],
}

pub(crate) struct ReceivedMessage {
    pub(crate) count: usize,
    pub(crate) full_length: usize,
    pub(crate) source: Option<SocketAddress>,
    pub(crate) local_address: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SocketAddress {
    Unix(UnixAddress),
    Inet(InetAddress),
    Packet(PacketAddress),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketError {
    Invalid,
    NoMemory,
    AddressInUse,
    AddressNotAvailable,
    NotFound,
    NotConnected,
    AlreadyConnected,
    /// active connect 已启动，调用方应进入 writable/error wait。
    InProgress,
    /// 同一 endpoint 已有尚未完成的 active connect。
    AlreadyInProgress,
    ConnectionRefused,
    /// established stream 被 peer reset。
    ConnectionReset,
    NetworkUnreachable,
    DestinationRequired,
    MessageTooLarge,
    ProtocolNotSupported,
    OperationNotSupported,
    Again,
    BrokenPipe,
    PermissionDenied,
    NoDevice,
    WrongType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketPollState {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    pub(crate) hangup: bool,
    pub(crate) error: bool,
}

impl SocketPollState {
    pub(crate) const fn error() -> Self {
        Self {
            readable: false,
            writable: false,
            hangup: false,
            error: true,
        }
    }
}

enum SocketBackend {
    Unix(Arc<UnixSocket>),
    Inet(Arc<InetSocket>),
    Packet(Arc<PacketSocket>),
    /// AF_INET raw control fd；data plane 未开放时不复制 NetworkStack 协议状态。
    InterfaceControl,
}

/// @description OFD 唯一 socket backend facade；AF_UNIX/AF_INET adapter 不穿透 fs seam。
pub(crate) struct Socket {
    domain: SocketDomain,
    socket_type: SocketType,
    backend: SocketBackend,
}

/// @description AF_UNIX stream connect 所需的双向 Pipe 与 server notification 资源。
pub(crate) struct UnixConnectResources {
    pub(crate) server_notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    pub(crate) client_to_server: (Arc<PipeEnd>, Arc<PipeEnd>),
    pub(crate) server_to_client: (Arc<PipeEnd>, Arc<PipeEnd>),
}

impl Socket {
    /// @description 创建 AF_UNIX、AF_INET 或 AF_PACKET endpoint，并一次性校验组合。
    ///
    /// @param domain Linux socket domain。
    /// @param socket_type stream/datagram type。
    /// @param protocol 零或 domain 对应的标准 protocol number。
    /// @param notify 接入统一 poll wait owner 的 notification Pipe endpoints。
    /// @return 唯一 socket facade；不支持组合或内存不足返回 `SocketError`。
    pub(crate) fn new(
        domain: SocketDomain,
        socket_type: SocketType,
        protocol: usize,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, SocketError> {
        let backend = match (domain, socket_type, protocol) {
            (SocketDomain::Unix, SocketType::Stream | SocketType::Datagram, 0) => {
                SocketBackend::Unix(UnixSocket::new(socket_type, notify))
            }
            (SocketDomain::Inet, SocketType::Datagram, 0 | 17) => {
                SocketBackend::Inet(InetSocket::new(SocketType::Datagram, notify)?)
            }
            (SocketDomain::Inet, SocketType::Stream, 0 | 6) => {
                SocketBackend::Inet(InetSocket::new(SocketType::Stream, notify)?)
            }
            (SocketDomain::Inet, SocketType::Raw, 1) => {
                SocketBackend::Inet(InetSocket::new(SocketType::Raw, notify)?)
            }
            (SocketDomain::Packet, SocketType::Datagram, _) => {
                SocketBackend::Packet(PacketSocket::new(protocol, notify)?)
            }
            (SocketDomain::Inet, SocketType::Raw, 255) => SocketBackend::InterfaceControl,
            _ => return Err(SocketError::ProtocolNotSupported),
        };
        Ok(Arc::new(Self {
            domain,
            socket_type,
            backend,
        }))
    }

    fn from_unix(socket: Arc<UnixSocket>) -> Arc<Self> {
        Arc::new(Self {
            domain: SocketDomain::Unix,
            socket_type: socket.socket_type(),
            backend: SocketBackend::Unix(socket),
        })
    }

    pub(crate) fn domain(&self) -> SocketDomain {
        self.domain
    }

    pub(crate) fn socket_type(&self) -> SocketType {
        self.socket_type
    }

    pub(crate) fn bind(self: &Arc<Self>, address: SocketAddress) -> Result<(), SocketError> {
        match (&self.backend, address) {
            (SocketBackend::Unix(socket), SocketAddress::Unix(address)) => socket.bind(address),
            (SocketBackend::Inet(socket), SocketAddress::Inet(address)) => socket.bind(address),
            (SocketBackend::Packet(socket), SocketAddress::Packet(address)) => socket.bind(address),
            (SocketBackend::InterfaceControl, _) => Err(SocketError::OperationNotSupported),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.listen(backlog),
            SocketBackend::Inet(socket) => socket.listen(backlog),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn connect(
        self: &Arc<Self>,
        address: SocketAddress,
        resources: Option<UnixConnectResources>,
    ) -> Result<(), SocketError> {
        match (&self.backend, address) {
            (SocketBackend::Inet(socket), SocketAddress::Inet(address)) => socket.connect(address),
            (SocketBackend::Unix(client), SocketAddress::Unix(address)) => {
                let listener = UnixSocket::lookup(&address)?;
                if self.socket_type == SocketType::Datagram {
                    return client.connect_datagram(&listener);
                }
                let resources = resources.ok_or(SocketError::NoMemory)?;
                let server = UnixSocket::new(SocketType::Stream, resources.server_notify);
                UnixSocket::connect_stream(
                    client,
                    &listener,
                    server,
                    resources.client_to_server,
                    resources.server_to_client,
                )
            }
            (SocketBackend::InterfaceControl, _) => Err(SocketError::OperationNotSupported),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn pair(
        first: &Arc<Self>,
        second: &Arc<Self>,
        first_to_second: (Arc<PipeEnd>, Arc<PipeEnd>),
        second_to_first: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<(), SocketError> {
        let (SocketBackend::Unix(first), SocketBackend::Unix(second)) =
            (&first.backend, &second.backend)
        else {
            return Err(SocketError::OperationNotSupported);
        };
        UnixSocket::pair(first, second, first_to_second, second_to_first)
    }

    /// @description 从 listener 接受连接，并为 AF_INET accepted endpoint 注入独立 wait source。
    /// @param notify AF_INET notification Pipe；AF_UNIX 使用 connect 时已建立的 transport。
    /// @return 新 Socket facade。
    /// @errors 暂无连接、状态无效或资源不足时返回错误。
    pub(crate) fn accept_with_notify(
        &self,
        notify: Option<(Arc<PipeEnd>, Arc<PipeEnd>)>,
    ) -> Result<Arc<Self>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.accept().map(Self::from_unix),
            SocketBackend::Inet(socket) => {
                let socket = socket.accept(notify.ok_or(SocketError::NoMemory)?)?;
                Ok(Arc::new(Self {
                    domain: SocketDomain::Inet,
                    socket_type: SocketType::Stream,
                    backend: SocketBackend::Inet(socket),
                }))
            }
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }

    /// @description 解析可能异步完成的 domain connect 结果。
    /// @return 已完成连接返回 unit。
    /// @errors 返回进行中、拒绝或未连接错误。
    pub(crate) fn connection_result(&self) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.connection_result(),
            SocketBackend::Unix(_) => Ok(()),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::InterfaceControl => Ok(()),
        }
    }

    /// @description 原子读取并清除 domain pending error。
    /// @return pending error；没有时为 None。
    /// @errors 无错误。
    pub(crate) fn take_error(&self) -> Option<SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.take_error(),
            SocketBackend::Unix(_) => None,
            SocketBackend::Packet(_) => None,
            SocketBackend::InterfaceControl => None,
        }
    }

    pub(crate) fn address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Packet(value))),
            SocketBackend::InterfaceControl => Ok(Some(SocketAddress::Inet(InetAddress {
                address: Ipv4Addr::UNSPECIFIED,
                port: 0,
            }))),
        }
    }

    pub(crate) fn peer_address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.peer_address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .peer_address()
                .map(|value| Some(SocketAddress::Inet(value))),
            SocketBackend::Packet(_) => Err(SocketError::NotConnected),
            SocketBackend::InterfaceControl => Err(SocketError::NotConnected),
        }
    }

    pub(crate) fn read(&self, output: &mut [u8]) -> Result<usize, SocketError> {
        self.receive(output).map(|(count, _)| count)
    }

    pub(crate) fn receive(
        &self,
        output: &mut [u8],
    ) -> Result<(usize, Option<SocketAddress>), SocketError> {
        self.receive_message(output, false)
            .map(|message| (message.count, message.source))
    }

    pub(crate) fn receive_message(
        &self,
        output: &mut [u8],
        peek: bool,
    ) -> Result<ReceivedMessage, SocketError> {
        match &self.backend {
            SocketBackend::Unix(_) if peek => Err(SocketError::OperationNotSupported),
            SocketBackend::Unix(socket) => {
                socket
                    .receive(output)
                    .map(|(count, source)| ReceivedMessage {
                        count,
                        full_length: count,
                        source: source.map(SocketAddress::Unix),
                        local_address: None,
                    })
            }
            SocketBackend::Inet(socket) => {
                socket
                    .receive(output, peek)
                    .map(
                        |(count, full_length, source, local_address)| ReceivedMessage {
                            count,
                            full_length,
                            source: Some(SocketAddress::Inet(source)),
                            local_address,
                        },
                    )
            }
            SocketBackend::Packet(socket) => {
                socket
                    .receive(output, peek)
                    .map(|(count, full_length, source)| ReceivedMessage {
                        count,
                        full_length,
                        source: Some(SocketAddress::Packet(source)),
                        local_address: None,
                    })
            }
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn set_ipv4_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_packet_info(enabled),
            SocketBackend::Unix(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }

    /// @description 将 SO_REUSEADDR policy 提交给具体 endpoint owner。
    /// @param enabled 非零 userspace option value 的布尔投影。
    /// @return AF_INET endpoint 成功更新返回 unit。
    /// @errors 不支持的 domain 或失效 endpoint 返回错误。
    pub(crate) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_reuse_address(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// @description 将 SO_BROADCAST policy 提交给 UDP endpoint owner。
    /// @param enabled 非零 userspace option value 的布尔投影。
    /// @return AF_INET UDP endpoint 成功更新返回 unit。
    /// @errors 不支持的 domain/type 或失效 endpoint 返回错误。
    pub(crate) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_broadcast(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// @description 将 SO_BINDTODEVICE interface name 提交给 endpoint owner。
    /// @param name 已去除 NUL 的 interface name；空值解除绑定。
    /// @return 当前唯一 eth0 binding 成功更新返回 unit。
    /// @errors 未知 interface、domain 或失效 endpoint 返回错误。
    pub(crate) fn bind_to_device(&self, name: &[u8]) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.bind_to_device(name),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// @description 设置 AF_INET raw endpoint 的 IPv4 hop limit。
    /// @param value 已按 Linux `IP_TTL` 约束验证的 1..=255 值。
    /// @return raw ICMP endpoint 成功提交返回 unit。
    /// @errors 非 AF_INET endpoint 或失效 endpoint 返回错误。
    pub(crate) fn set_ipv4_hop_limit(&self, value: u8) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_hop_limit(value),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn set_tcp_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_no_delay(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn ipv4_packet_info(&self) -> bool {
        matches!(&self.backend, SocketBackend::Inet(socket) if socket.packet_info())
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.write(input),
            SocketBackend::Inet(socket) => socket.send_to(input, None),
            SocketBackend::Packet(socket) => socket.send_to(input, None),
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn send_to(
        &self,
        input: &[u8],
        target: Option<SocketAddress>,
    ) -> Result<usize, SocketError> {
        match (&self.backend, target) {
            (SocketBackend::Unix(socket), Some(SocketAddress::Unix(address))) => {
                let target = UnixSocket::lookup(&address)?;
                socket.send_to(input, Some(&target))
            }
            (SocketBackend::Unix(socket), None) => socket.send_to(input, None),
            (SocketBackend::Inet(socket), Some(SocketAddress::Inet(address))) => {
                socket.send_to(input, Some(address))
            }
            (SocketBackend::Inet(socket), None) => socket.send_to(input, None),
            (SocketBackend::Packet(socket), Some(SocketAddress::Packet(address))) => {
                socket.send_to(input, Some(address))
            }
            (SocketBackend::Packet(socket), None) => socket.send_to(input, None),
            (SocketBackend::InterfaceControl, _) => Err(SocketError::OperationNotSupported),
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.poll_state(),
            SocketBackend::Inet(socket) => socket.poll_state(),
            SocketBackend::Packet(socket) => socket.poll_state(),
            SocketBackend::InterfaceControl => SocketPollState {
                readable: false,
                writable: true,
                hangup: false,
                error: false,
            },
        }
    }

    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.readiness_generation(events),
            SocketBackend::Inet(socket) => socket.readiness_generation(),
            SocketBackend::Packet(socket) => socket.readiness_generation(),
            SocketBackend::InterfaceControl => 0,
        }
    }

    pub(crate) fn wait_pipes(&self) -> Vec<(Arc<Pipe>, PipeDirection)> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.wait_pipes(),
            SocketBackend::Inet(socket) => socket.wait_pipes(),
            SocketBackend::Packet(socket) => socket.wait_pipes(),
            SocketBackend::InterfaceControl => Vec::new(),
        }
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.shutdown(how),
            SocketBackend::Inet(socket) => socket.shutdown(how),
            SocketBackend::Packet(_) => Err(SocketError::OperationNotSupported),
            SocketBackend::InterfaceControl => Err(SocketError::OperationNotSupported),
        }
    }
}

pub(crate) fn init() {
    packet::init();
    inet::init();
}
