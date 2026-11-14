//! UDP 地址族、mapped 地址与 zone 的规范化；端口索引与 wire 均消费同一身份。
use super::*;

pub(super) fn wire_ip(address: InetAddress) -> IpAddr {
    match address.ip() {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        ip => ip,
    }
}
pub(super) fn wire_address(address: InetAddress) -> InetAddress {
    InetAddress::new(wire_ip(address), address.port())
}

pub(super) fn present_address(
    ipv6: bool,
    address: InetAddress,
    interface: Option<u32>,
) -> InetAddress {
    if !ipv6 {
        return wire_address(address);
    }
    let ip = match address.ip() {
        IpAddr::V4(ip) => ip.to_ipv6_mapped(),
        IpAddr::V6(ip) => ip,
    };
    let scope = if ip.is_unicast_link_local() || ip.is_multicast() && ip.octets()[1] & 15 <= 2 {
        interface.unwrap_or(0)
    } else {
        0
    };
    InetAddress::V6(SocketAddrV6::new(ip, address.port(), 0, scope))
}

impl EndpointState {
    pub(super) fn binding(
        &self,
        address: InetAddress,
        interface: Option<u32>,
    ) -> Result<PortBinding, SocketError> {
        PortBinding::new(address.ip(), self.options.ipv6_only)
            .map(|binding| binding.with_interface(interface))
            .map_err(port_error)
    }
    pub(super) fn request(&self, address: InetAddress) -> Result<InetAddress, SocketError> {
        if self.local.is_ipv4() && address.is_ipv6() {
            return Err(SocketError::AddressFamilyNotSupported);
        }
        if self.local.is_ipv6() && self.options.ipv6_only && wire_ip(address).is_ipv4() {
            return Err(if address.is_ipv4() {
                SocketError::AddressFamilyNotSupported
            } else {
                SocketError::NetworkUnreachable
            });
        }
        Ok(address)
    }
    pub(super) fn destination(&self, address: InetAddress) -> Result<InetAddress, SocketError> {
        let address = self.request(address)?;
        if !wire_ip(address).is_unspecified() {
            return Ok(address);
        }
        let ip = if wire_ip(address).is_ipv4() || wire_ip(self.local).is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        };
        Ok(self.present(InetAddress::new(ip, address.port()), None))
    }
    pub(super) fn present(&self, address: InetAddress, interface: Option<u32>) -> InetAddress {
        present_address(self.local.is_ipv6(), address, interface)
    }
    pub(super) fn destination_interface(
        &self,
        address: InetAddress,
    ) -> Result<Option<u32>, SocketError> {
        if let InetAddress::V6(address) = address
            && (address.ip().is_unicast_link_local()
                || address.ip().is_multicast() && address.ip().octets()[1] & 15 <= 2)
        {
            let zone = (address.scope_id() != 0).then_some(address.scope_id());
            if zone.is_some()
                && self.options.bound_to_device.is_some()
                && zone != self.options.bound_to_device
            {
                return Err(SocketError::Invalid);
            }
            let outgoing = zone.or(self.options.bound_to_device);
            return if address.ip().is_unicast_link_local() {
                outgoing.map(Some).ok_or(SocketError::Invalid)
            } else {
                Ok(outgoing)
            };
        }
        Ok(self.options.bound_to_device)
    }
}

impl EndpointState {
    /// 实际 wire 族确定之后逐项验证；不把单次出口/源覆盖写回 socket 绑定。
    pub(super) fn send_packet_info(
        &self,
        network: &NetworkStack,
        peer: InetAddress,
        controls: &[network_protocol::InternetSendPacketInfo],
    ) -> Result<(IpAddr, Option<u32>), SocketError> {
        if wire_ip(peer).is_ipv4() {
            let bound = match wire_ip(self.local) {
                IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_multicast() => Some(ip),
                IpAddr::V6(ip) if !ip.is_unspecified() => {
                    return Err(SocketError::NetworkUnreachable);
                }
                _ => None,
            };
            let (source, outgoing) = network.ipv4_packet_info(
                controls,
                self.is_ipv6(),
                bound,
                &self.options,
                match wire_ip(peer) {
                    IpAddr::V4(ip) => ip,
                    _ => unreachable!(),
                },
            )?;
            return Ok((
                IpAddr::V4(source.unwrap_or(Ipv4Addr::UNSPECIFIED)),
                outgoing,
            ));
        }
        let InetAddress::V6(peer) = peer else {
            return Err(SocketError::AddressFamilyNotSupported);
        };
        let IpAddr::V6(bound) = wire_ip(self.local) else {
            return Err(SocketError::NetworkUnreachable);
        };
        network.ipv6_packet_info(
            controls,
            bound,
            self.options.bound_to_device,
            self.options.ipv6_multicast_interface,
            peer,
        )
    }
}
