//! One bounded exchange with a configured resolver, sharing the actor's port.

use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use dns_cache::Query;
use native_runtime::system::{cprng_draw, monotonic};
use network_protocol::{InetAddress, InetProtocol, RemoteNamespace, SocketError};
use packet_wire::dns::FLAG_TC;

use crate::socket::{DNS_MAX, Socket, StreamInput, StreamOutput};

pub(super) const UDP_LIMIT: usize = 1232;
const UDP_TIMEOUT: i64 = 1_500_000_000;
const TCP_TIMEOUT: i64 = 2_000_000_000;

pub(super) struct Attempt {
    pub(super) socket: Socket,
    pub(super) deadline: i64,
    pub(super) id: u16,
    peer: InetAddress,
    exchange: Exchange,
}

enum Exchange {
    Datagram {
        sent: bool,
        edns_size: Option<usize>,
    },
    Stream {
        connecting: bool,
        output: Option<StreamOutput>,
        input: StreamInput,
    },
}

pub(super) enum Progress {
    Wait,
    Continue,
    RetryTcp,
    Complete(Vec<u8>),
}

impl Attempt {
    pub(super) fn open(
        network: &RemoteNamespace,
        peer: InetAddress,
        tcp: bool,
        request: &mut [u8],
    ) -> Result<Self, SocketError> {
        let mut random = [0; 2];
        cprng_draw(&mut random);
        let id = u16::from_ne_bytes(random);
        let protocol = if tcp {
            InetProtocol::Tcp
        } else {
            InetProtocol::Udp
        };
        let socket = Socket::open(network, peer, protocol)?;
        let exchange = if tcp {
            let connecting = match socket.endpoint.connect(peer) {
                Ok(()) => false,
                Err(SocketError::InProgress) => true,
                Err(error) => return Err(error),
            };
            Exchange::Stream {
                connecting,
                output: Some(with_id(request, id, StreamOutput::new)?),
                input: StreamInput::default(),
            }
        } else {
            bind_random(&socket, peer)?;
            socket.endpoint.connect(peer)?;
            Exchange::Datagram {
                sent: false,
                edns_size: Query::parse(request)
                    .map_err(|_| SocketError::ProtocolError)?
                    .udp_size_offset(),
            }
        };
        let timeout = if tcp { TCP_TIMEOUT } else { UDP_TIMEOUT };
        Ok(Self {
            socket,
            deadline: monotonic().saturating_add(timeout),
            id,
            peer,
            exchange,
        })
    }

    pub(super) fn drive(&mut self, request: &mut [u8]) -> Result<Progress, SocketError> {
        match &mut self.exchange {
            Exchange::Datagram { sent, edns_size } => {
                if !*sent {
                    let length = request.len();
                    let saved = edns_size.map(|offset| {
                        let original = [request[offset], request[offset + 1]];
                        let safe = u16::from_be_bytes(original).clamp(512, UDP_LIMIT as u16);
                        request[offset..offset + 2].copy_from_slice(&safe.to_be_bytes());
                        (offset, original)
                    });
                    let result = with_id(request, self.id, |bytes| self.socket.send(bytes, None));
                    if let Some((offset, original)) = saved {
                        request[offset..offset + 2].copy_from_slice(&original);
                    }
                    match result {
                        Ok(actual) if actual == length => *sent = true,
                        Ok(_) => return Err(SocketError::BrokenPipe),
                        Err(SocketError::Again | SocketError::NoBuffer) => {
                            return Ok(Progress::Wait);
                        }
                        Err(error) => return Err(error),
                    }
                }
                let query = Query::parse(request).map_err(|_| SocketError::ProtocolError)?;
                for _ in 0..8 {
                    let datagram = match self.socket.endpoint.receive(DNS_MAX, false) {
                        Ok(datagram) => datagram,
                        Err(SocketError::Again) => return Ok(Progress::Wait),
                        Err(error) => return Err(error),
                    };
                    if !datagram.source.is_some_and(|source| {
                        source.ip() == self.peer.ip() && source.port() == self.peer.port()
                    }) || datagram.full_length != datagram.data.len()
                    {
                        continue;
                    }
                    if let Ok(header) = query.match_reply(self.id, &datagram.data) {
                        if header.flags & FLAG_TC != 0 {
                            return Ok(Progress::RetryTcp);
                        }
                        if query.verify_reply(self.id, &datagram.data).is_ok() {
                            return Ok(Progress::Complete(datagram.data));
                        }
                    }
                }
                Ok(Progress::Continue)
            }
            Exchange::Stream {
                connecting,
                output,
                input,
            } => {
                if *connecting {
                    match self.socket.endpoint.connection_result() {
                        Ok(()) => *connecting = false,
                        Err(SocketError::InProgress) => return Ok(Progress::Wait),
                        Err(error) => return Err(error),
                    }
                }
                if let Some(message) = output {
                    if !message.flush(&self.socket)? {
                        return Ok(Progress::Wait);
                    }
                    *output = None;
                }
                Ok(input
                    .receive(&self.socket)?
                    .map_or(Progress::Wait, Progress::Complete))
            }
        }
    }
}

fn with_id<T>(bytes: &mut [u8], id: u16, consume: impl FnOnce(&[u8]) -> T) -> T {
    let original = [bytes[0], bytes[1]];
    bytes[..2].copy_from_slice(&id.to_be_bytes());
    let result = consume(bytes);
    bytes[..2].copy_from_slice(&original);
    result
}

fn bind_random(socket: &Socket, peer: InetAddress) -> Result<(), SocketError> {
    let local = if peer.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    // NET's ordinary ephemeral allocator is deterministic. DNS uses a fresh random port per
    // transaction in addition to a fresh random ID, with no predictable fallback on collision.
    for _ in 0..16 {
        let mut bytes = [0; 2];
        cprng_draw(&mut bytes);
        let port = u16::from_ne_bytes(bytes);
        if port < 1024 {
            continue;
        }
        match socket.endpoint.bind(InetAddress::new(local, port)) {
            Ok(()) => return Ok(()),
            Err(SocketError::AddressInUse) => {}
            Err(error) => return Err(error),
        }
    }
    Err(SocketError::AddressInUse)
}
