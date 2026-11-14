//! 每地址族共用一对 RAW 队列，通过 packet info 保持接口和选源身份。

use super::authentication::{Authentication, SendSequence};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV6};
use native_runtime::{
    Event,
    native_abi::{self, Status},
};
use network_protocol::{
    InetAddress, InetFamily, InetOptionChange, InetProtocol, InetReceived, InternetSendPacketInfo,
    RemoteInetSocket, RemoteNamespace, SocketError,
};
use packet_wire::{
    PacketError,
    ospf::{self, ChecksumContext, Header, Kind, PROTOCOL, Version},
};

pub(super) const ALL_V2: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 5);
pub(super) const ALL_V3: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 5);

pub(super) struct Wire<'a> {
    pub(super) socket: &'a Socket,
    pub(super) index: u32,
    pub(super) source: IpAddr,
    pub(super) mtu: u16,
    pub(super) header: Header,
    pub(super) authentication: Option<&'a Authentication>,
    pub(super) sequence: &'a mut SendSequence,
    pub(super) buffer: &'a mut Vec<u8>,
}

impl Wire<'_> {
    pub(super) fn body_limit(&self) -> usize {
        let ip_header = if self.source.is_ipv4() { 20 } else { 40 };
        usize::from(self.mtu)
            - ip_header
            - self.header.version().header_len()
            - self.authentication_overhead()
    }

    pub(super) fn authentication_overhead(&self) -> usize {
        self.authentication
            .map_or(0, |_| Authentication::overhead(self.header.version()))
    }

    pub(super) fn send(
        &mut self,
        kind: Kind,
        body_len: usize,
        write: impl FnOnce(&mut [u8]) -> Result<usize, PacketError>,
    ) -> Result<(), Status> {
        let version = self.header.version();
        let head = version.header_len();
        let length = head + body_len;
        let wire_length = length + self.authentication_overhead();
        self.buffer
            .try_reserve(wire_length.saturating_sub(self.buffer.len()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.buffer.resize(wire_length, 0);
        let body_len =
            write(&mut self.buffer[head..length]).map_err(|_| native_abi::ERR_BAD_STATE)?;
        let length = ospf::write_header(self.buffer, self.header, kind, body_len)
            .map_err(|_| native_abi::ERR_BAD_STATE)?;
        let checksum = match self.source {
            IpAddr::V4(_) => ChecksumContext::V2,
            IpAddr::V6(source) => ChecksumContext::V3 {
                source,
                destination: ALL_V3,
            },
        };
        let wire_length = length + self.authentication_overhead();
        if let Some(authentication) = self.authentication {
            authentication.sign(
                version,
                kind,
                self.source,
                self.sequence,
                length,
                &mut self.buffer[..wire_length],
            )?;
        } else {
            ospf::write_checksum(&mut self.buffer[..length], checksum)
                .map_err(|_| native_abi::ERR_BAD_STATE)?;
        }
        match self
            .socket
            .send(self.index, self.source, &self.buffer[..wire_length])
        {
            Err(SocketError::NetworkDown | SocketError::BrokenPipe) => {
                Err(native_abi::ERR_PEER_CLOSED)
            }
            // 链路暂态错误交给相应协议定时器重发。
            _ => Ok(()),
        }
    }
}

pub(super) struct Socket {
    endpoint: RemoteInetSocket,
    pub(super) ready: Event,
}

impl Socket {
    pub(super) fn open(network: &RemoteNamespace, version: Version) -> Result<Self, Status> {
        let family = match version {
            Version::V2 => InetFamily::Ipv4,
            Version::V3 => InetFamily::Ipv6,
        };
        let endpoint = network
            .socket(family, InetProtocol::Raw(PROTOCOL))
            .map_err(status)?;
        let socket = RemoteInetSocket::new(endpoint.channel).map_err(status)?;
        let options = match version {
            Version::V2 => [
                InetOptionChange::PacketInfo(true),
                InetOptionChange::Ipv4MulticastAll(false),
                InetOptionChange::Ipv4MulticastLoop(false),
                InetOptionChange::Ipv4MulticastHopLimit(Some(1)),
            ],
            Version::V3 => [
                InetOptionChange::Ipv6PacketInfo(true),
                InetOptionChange::Ipv6MulticastAll(false),
                InetOptionChange::Ipv6MulticastLoop(false),
                InetOptionChange::Ipv6MulticastHopLimit(Some(1)),
            ],
        };
        for option in options {
            socket.set_option(option).map_err(status)?;
        }
        Ok(Self {
            endpoint: socket,
            ready: endpoint.readiness,
        })
    }

    pub(super) fn membership(
        &self,
        version: Version,
        interface: u32,
        join: bool,
    ) -> Result<(), SocketError> {
        self.endpoint.set_option(match version {
            Version::V2 => InetOptionChange::Ipv4Membership {
                group: ALL_V2,
                interface,
                address: Ipv4Addr::UNSPECIFIED,
                join,
            },
            Version::V3 => InetOptionChange::Ipv6Membership {
                group: ALL_V3,
                interface,
                join,
            },
        })
    }

    pub(super) fn send(
        &self,
        interface: u32,
        source: IpAddr,
        bytes: &[u8],
    ) -> Result<(), SocketError> {
        let (destination, control) = match source {
            IpAddr::V4(address) => (
                InetAddress::new(ALL_V2.into(), 0),
                InternetSendPacketInfo::Ipv4 {
                    interface_index: Some(interface),
                    source_address: address,
                },
            ),
            IpAddr::V6(address) => (
                InetAddress::V6(SocketAddrV6::new(ALL_V3, 0, 0, interface)),
                InternetSendPacketInfo::Ipv6 {
                    interface_index: Some(interface),
                    source_address: address,
                },
            ),
        };
        let sent = self
            .endpoint
            .send_to(bytes, Some(destination), core::iter::once(control))?;
        if sent == bytes.len() {
            Ok(())
        } else {
            Err(SocketError::ProtocolError)
        }
    }

    pub(super) fn receive(&self) -> Result<Option<InetReceived>, SocketError> {
        match self.endpoint.receive(u16::MAX as usize, false) {
            Ok(packet) => Ok(Some(packet)),
            Err(SocketError::Again) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub(super) fn status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::NetworkDown | SocketError::BrokenPipe => native_abi::ERR_PEER_CLOSED,
        _ => native_abi::ERR_IO,
    }
}
