//! Direct Ethernet delivery keeps the server usable before a client has an ARP entry or IPv4 address.

use super::{Action, Delivery, Error, REPLY_BYTES, Reply, Request, Server};
use core::net::Ipv4Addr;
use packet_wire::{
    dhcpv4::{CLIENT_PORT, MessageType, SERVER_PORT},
    ipv4::{Ipv4Header, Ipv4Packet},
    udp::{self, UdpPacket},
};

const IP_BYTES: usize = 20;
const HEADERS: usize = IP_BYTES + 8;

/// IPv4 and UDP headers plus the maximum DHCP reply body.
pub const DATAGRAM_BYTES: usize = HEADERS + REPLY_BYTES;

impl Server {
    /// Process an IPv4 datagram received on this pool's link.
    /// # Parameters
    /// `source_mac` is the link-layer source, `now_ms` is monotonic time,
    /// and `output` is caller-owned storage for the complete IP response.
    /// # Returns
    /// A response or an address to probe, or None if no transmission is needed.
    /// # Errors
    /// Malformed IP, UDP or DHCP framing, or insufficient resources for a new offer.
    pub fn handle<'a>(
        &mut self,
        datagram: &[u8],
        source_mac: [u8; 6],
        now_ms: u64,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Option<Action<'a>>, Error> {
        let ip = Ipv4Packet::parse(datagram)?;
        let subnet_broadcast = Ipv4Addr::from(u32::from(self.pool.server) | !self.mask);
        if ip.protocol() != 17
            || ip.fragmented()
            || ![self.pool.server, Ipv4Addr::BROADCAST, subnet_broadcast]
                .contains(&ip.destination())
        {
            return Ok(None);
        }
        let udp = UdpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into())?;
        if udp.source_port() != CLIENT_PORT || udp.destination_port() != SERVER_PORT {
            return Ok(None);
        }
        self.handle_payload(udp.payload(), ip.source(), source_mac, now_ms, output)
    }

    pub(super) fn datagram<'a>(
        &self,
        request: &Request,
        kind: MessageType,
        address: Ipv4Addr,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Reply<'a>, Error> {
        let body = <&mut [u8; REPLY_BYTES]>::try_from(&mut output[HEADERS..])
            .expect("fixed DHCP body capacity");
        let reply = self.reply(request, kind, address, body)?;
        let length = HEADERS + reply.bytes.len();
        let delivery = reply.delivery;
        let destination = match delivery {
            Delivery::Broadcast => Ipv4Addr::BROADCAST,
            Delivery::Unicast { address, .. } => address,
        };
        let response = &mut output[..length];
        udp::emit(
            &mut response[IP_BYTES..],
            self.pool.server.into(),
            destination.into(),
            SERVER_PORT,
            CLIENT_PORT,
        )?;
        Ipv4Header {
            source: self.pool.server,
            destination,
            protocol: 17,
            hop_limit: 64,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0x4000,
            options: &[],
        }
        .emit(response)?;
        Ok(Reply {
            delivery,
            bytes: response,
        })
    }
}
