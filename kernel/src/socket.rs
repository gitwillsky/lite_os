use alloc::{sync::Arc, vec::Vec};
use core::net::Ipv4Addr;

use crate::ipc::{Pipe, PipeDirection, PipeEnd};

#[path = "socket/inet.rs"]
mod inet;
#[path = "socket/unix.rs"]
mod unix;

use inet::InetSocket;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketType {
    Stream,
    Datagram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InetAddress {
    pub(crate) address: Ipv4Addr,
    pub(crate) port: u16,
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
    ConnectionRefused,
    NetworkUnreachable,
    DestinationRequired,
    MessageTooLarge,
    ProtocolNotSupported,
    OperationNotSupported,
    Again,
    BrokenPipe,
    WrongType,
}

#[derive(Debug, Clone, Copy)]
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
    /// @description 创建 AF_UNIX 或 AF_INET endpoint，并一次性校验 domain/type/protocol。
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
            (SocketDomain::Unix, _, 0) => SocketBackend::Unix(UnixSocket::new(socket_type, notify)),
            (SocketDomain::Inet, SocketType::Datagram, 0 | 17) => {
                SocketBackend::Inet(InetSocket::new(notify)?)
            }
            (SocketDomain::Inet, SocketType::Stream, _) => {
                return Err(SocketError::ProtocolNotSupported);
            }
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
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.listen(backlog),
            SocketBackend::Inet(_) => Err(SocketError::OperationNotSupported),
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

    pub(crate) fn accept(&self) -> Result<Arc<Self>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.accept().map(Self::from_unix),
            SocketBackend::Inet(_) => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .address()
                .map(|value| Some(SocketAddress::Inet(value))),
        }
    }

    pub(crate) fn peer_address(&self) -> Result<Option<SocketAddress>, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.peer_address().map(SocketAddress::Unix)),
            SocketBackend::Inet(socket) => socket
                .peer_address()
                .map(|value| Some(SocketAddress::Inet(value))),
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
        }
    }

    pub(crate) fn set_ipv4_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => {
                socket.set_packet_info(enabled);
                Ok(())
            }
            SocketBackend::Unix(_) => Err(SocketError::OperationNotSupported),
        }
    }

    pub(crate) fn ipv4_packet_info(&self) -> bool {
        matches!(&self.backend, SocketBackend::Inet(socket) if socket.packet_info())
    }

    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.write(input),
            SocketBackend::Inet(socket) => socket.send_to(input, None),
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
            _ => Err(SocketError::Invalid),
        }
    }

    pub(crate) fn poll_state(&self) -> SocketPollState {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.poll_state(),
            SocketBackend::Inet(socket) => socket.poll_state(),
        }
    }

    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.readiness_generation(events),
            SocketBackend::Inet(socket) => socket.readiness_generation(),
        }
    }

    pub(crate) fn wait_pipes(&self) -> Vec<(Arc<Pipe>, PipeDirection)> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.wait_pipes(),
            SocketBackend::Inet(socket) => socket.wait_pipes(),
        }
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.shutdown(how),
            SocketBackend::Inet(_) => Err(SocketError::OperationNotSupported),
        }
    }
}

pub(crate) fn init() {
    inet::init();
}
