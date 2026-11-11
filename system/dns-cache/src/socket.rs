//! Native endpoint readiness and RFC 1035 length-prefixed TCP I/O.

use alloc::vec::Vec;

use native_runtime::{
    Event, Port,
    native_abi::{Status, packet, signals},
};
use network_protocol::{
    InetAddress, InetFamily, InetProtocol, RemoteInetSocket, RemoteNamespace, SocketEndpoint,
    SocketError,
};

pub(super) const DNS_MAX: usize = u16::MAX as usize;

pub(super) struct Socket {
    pub(super) endpoint: RemoteInetSocket,
    ready: Event,
    armed: bool,
    scheduled: bool,
}

impl Socket {
    pub(super) fn open(
        network: &RemoteNamespace,
        address: InetAddress,
        protocol: InetProtocol,
    ) -> Result<Self, SocketError> {
        let family = if address.is_ipv4() {
            InetFamily::Ipv4
        } else {
            InetFamily::Ipv6
        };
        Self::new(network.socket(family, protocol)?)
    }

    pub(super) fn new(endpoint: SocketEndpoint) -> Result<Self, SocketError> {
        Ok(Self {
            endpoint: RemoteInetSocket::new(endpoint.channel)?,
            ready: endpoint.readiness,
            armed: false,
            scheduled: false,
        })
    }

    pub(super) fn arm(&mut self, port: &Port, key: u64) -> Result<(), Status> {
        // The native wait is one-shot. Reuse it until consumed to avoid duplicate queued wakes.
        if !self.armed {
            self.ready.wait_async(port, key, signals::USER_0)?;
            self.armed = true;
        }
        Ok(())
    }

    pub(super) fn woke(&mut self, kind: u32) -> Result<(), Status> {
        if kind == packet::SIGNAL_ONE {
            self.armed = false;
            self.ready.signal(signals::USER_0, 0)?;
        } else {
            self.scheduled = false;
        }
        Ok(())
    }

    pub(super) fn schedule(&mut self, port: &Port, key: u64) -> Result<(), Status> {
        // Budget continuation has one outstanding packet independent of endpoint readiness.
        if !self.scheduled {
            port.queue(key, 0)?;
            self.scheduled = true;
        }
        Ok(())
    }

    pub(super) fn send(
        &self,
        bytes: &[u8],
        target: Option<InetAddress>,
    ) -> Result<usize, SocketError> {
        self.endpoint.send_to(bytes, target, core::iter::empty())
    }
}

#[derive(Default)]
pub(super) struct StreamInput {
    bytes: Vec<u8>,
    length: Option<usize>,
}

impl StreamInput {
    pub(super) fn receive(&mut self, socket: &Socket) -> Result<Option<Vec<u8>>, SocketError> {
        loop {
            let needed = self.length.unwrap_or(2) - self.bytes.len();
            if needed == 0 {
                if self.length.take().is_some() {
                    return Ok(Some(core::mem::take(&mut self.bytes)));
                }
                let length = usize::from(u16::from_be_bytes([self.bytes[0], self.bytes[1]]));
                if length < packet_wire::dns::HEADER_BYTES {
                    return Err(SocketError::ProtocolError);
                }
                self.bytes.clear();
                self.length = Some(length);
                continue;
            }
            match socket.endpoint.receive(needed, false) {
                Ok(received) if received.data.is_empty() => return Err(SocketError::BrokenPipe),
                Ok(received) => {
                    if self.bytes.is_empty() && self.length == Some(received.data.len()) {
                        self.length = None;
                        return Ok(Some(received.data));
                    }
                    self.bytes
                        .try_reserve(received.data.len())
                        .map_err(|_| SocketError::NoMemory)?;
                    self.bytes.extend_from_slice(&received.data);
                }
                Err(SocketError::Again) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) struct StreamOutput {
    bytes: Vec<u8>,
    sent: usize,
}

impl StreamOutput {
    pub(super) fn new(message: &[u8]) -> Result<Self, SocketError> {
        let length = u16::try_from(message.len()).map_err(|_| SocketError::MessageTooLarge)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(2 + message.len())
            .map_err(|_| SocketError::NoMemory)?;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(message);
        Ok(Self { bytes, sent: 0 })
    }

    pub(super) fn flush(&mut self, socket: &Socket) -> Result<bool, SocketError> {
        while self.sent < self.bytes.len() {
            match socket.send(&self.bytes[self.sent..], None) {
                Ok(0) => return Err(SocketError::BrokenPipe),
                Ok(sent) => self.sent += sent,
                Err(SocketError::Again) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }
}
