use super::*;

#[cfg(feature = "socket-dns")]
use crate::socket::dns::Socket as DnsSocket;

#[cfg(feature = "socket-udp")]
use crate::socket::udp::Socket as UdpSocket;

impl InterfaceInner {
    pub(super) fn process_udp<'frame>(
        &mut self,
        _sockets: &mut SocketSet,
        _meta: PacketMeta,
        handled_by_raw_socket: bool,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
        datagram: &mut impl FnMut(Ipv4Ingress<'_>) -> bool,
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        if datagram(Ipv4Ingress {
            packet:None,
            protocol:17, hop_limit:ip_repr.hop_limit(), handled_by_raw:handled_by_raw_socket,
            reverse_route:None, bound_reverse_route:None,
            source: src_addr,
            destination: dst_addr,
            specific_destination: self.ingress_spec_address(&ip_repr),
            interface_index: self.interface_index(),
            multicast: self.is_broadcast(&dst_addr) || dst_addr.is_multicast(),
            checksum_valid: _meta.rx_checksum_valid,
            payload: ip_payload,
        }) { return None; }
        let udp_packet = check!(UdpPacket::new_checked(ip_payload));
        let _udp_repr = check!(UdpRepr::parse(&udp_packet, &src_addr, &dst_addr, &self.caps.checksum));

        #[cfg(feature = "socket-udp")]
        for udp_socket in _sockets
            .items_mut()
            .filter_map(|i| UdpSocket::downcast_mut(&mut i.socket))
        {
            if udp_socket.accepts(self, &ip_repr, &_udp_repr) {
                udp_socket.process(self, _meta, &ip_repr, &_udp_repr, udp_packet.payload());
                return None;
            }
        }

        #[cfg(feature = "socket-dns")]
        for dns_socket in _sockets
            .items_mut()
            .filter_map(|i| DnsSocket::downcast_mut(&mut i.socket))
        {
            if dns_socket.accepts(&ip_repr, &_udp_repr) {
                dns_socket.process(self, &ip_repr, &_udp_repr, udp_packet.payload());
                return None;
            }
        }

        // The packet wasn't handled by a socket, send an ICMP port unreachable packet.
        match ip_repr {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(_) if handled_by_raw_socket => None,
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(_) if handled_by_raw_socket => None,
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(ipv4_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV4_MIN_MTU, ipv4_repr.buffer_len());
                let icmpv4_reply_repr = Icmpv4Repr::DstUnreachable {
                    reason: Icmpv4DstUnreachable::PortUnreachable,
                    header: ipv4_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv4_reply(ipv4_repr, icmpv4_reply_repr)
            }
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(ipv6_repr) => {
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV6_MIN_MTU, ipv6_repr.buffer_len());
                let icmpv6_reply_repr = Icmpv6Repr::DstUnreachable {
                    reason: Icmpv6DstUnreachable::PortUnreachable,
                    header: ipv6_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv6_reply(ipv6_repr, icmpv6_reply_repr)
            }
        }
    }
}
