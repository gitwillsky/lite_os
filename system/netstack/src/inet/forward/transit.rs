//! 选路只读当前策略和接口资格。所选路径仅供本次发送尝试使用，等候中的报文不保留它。

use super::*;
use crate::inet::route::{Ipv6Selection, RouteSelection, hash_ipv4, hash_ipv6};
use packet_wire::batch::NetworkPacket;

pub(super) type TransitPolicy<'a> = (u32, u32, &'a [router_protocol::policy_routing::Rule]);

#[derive(Clone, Copy)]
pub(super) enum TransitPath {
    Ipv4(RouteSelection),
    Ipv6(Ipv6Selection),
}

impl TransitPath {
    pub(super) fn lookup(
        ports: &Ports<'_>,
        routes: Routes<'_>,
        (source, destination): (IpAddr, IpAddr),
        now: Instant,
    ) -> Result<Self, SendResult> {
        let policy = (ports.current.index, ports.mark, ports.transit_rules(source));
        let probe = ports.prefix_probe(destination);
        match (source, destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => ipv4::select(
                routes.0,
                source,
                (destination, probe, ports.receive_destination),
                policy,
                ports.facts(),
            )
            .map(Self::Ipv4),
            (IpAddr::V6(source), IpAddr::V6(destination)) => ipv6::select(
                routes.1,
                (source, destination, probe),
                policy,
                ports.ipv6_facts(),
                now,
            )
            .map(Self::Ipv6),
            _ => unreachable!("transit addresses have the same family"),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum TransitPacket<'a> {
    Ipv4(Ipv4Packet<'a>),
    Ipv6(Ipv6Datagram<'a>),
}

impl<'a> TransitPacket<'a> {
    /// 出口更宽时，在入队列对应的发送队列子集内按原始报文散列。
    /// 等宽和更窄的出口沿用原队列映射；分片和不透明扩展头只使用地址。
    pub(super) fn transmit_queue(
        self,
        (device, ingress): (&ProtocolDevice, u16),
        transmit: u16,
    ) -> u16 {
        if transmit == 1 {
            return 0;
        }
        let receive = device.transmit_queue_count();
        if transmit <= receive {
            return ingress;
        }
        let ingress = if ingress < receive {
            ingress
        } else {
            ingress % receive
        };
        let members = (transmit - ingress).div_ceil(receive);
        if members == 1 {
            return ingress;
        }
        let packet = match self {
            Self::Ipv4(ip) => NetworkPacket::Ipv4(ip),
            Self::Ipv6(ip) => NetworkPacket::Ipv6(ip),
        };
        let ports = packet
            .direct_transport()
            .filter(|(_, _, protocol, _)| matches!(protocol, 6 | 17))
            .and_then(|(_, _, protocol, bytes)| {
                let ports = u32::from_be_bytes(*bytes.first_chunk::<4>()?);
                Some(u64::from(protocol) << 32 | u64::from(ports))
            })
            .unwrap_or(0);
        let hash = match self {
            Self::Ipv4(ip) => hash_ipv4(ip.source(), ip.destination(), ports),
            Self::Ipv6(ip) => hash_ipv6(ip.source(), ip.destination(), ports),
        };
        let bucket = ((u64::from(hash) * u64::from(members)) >> 32) as u16;
        ingress + receive * bucket
    }

    pub(super) fn from_validated(bytes: &'a [u8]) -> Self {
        match bytes[0] >> 4 {
            4 => Self::Ipv4(Ipv4Packet::parse_header(bytes).expect("validated IPv4")),
            6 => Self::Ipv6(Ipv6Datagram::parse(bytes).expect("validated IPv6")),
            _ => unreachable!("only IP enters forwarding"),
        }
    }

    pub(super) fn select(
        self,
        ports: &Ports<'_>,
        routes: Routes<'_>,
        (rewrite, path): (Option<Rewrite>, Option<TransitPath>),
        now: Instant,
    ) -> Result<TransitPath, SendResult> {
        if let Some(path) = path {
            return Ok(path);
        }
        // 策略与多路径使用入站源和转换后的目的，不能使用 SNAT 后的源改变出口。
        let addresses = match self {
            Self::Ipv4(ip) => {
                let destination = rewrite.map_or_else(|| ip.destination(), |edit| edit.ipv4(ip).1);
                (ip.source().into(), destination.into())
            }
            Self::Ipv6(ip) => {
                let destination = rewrite.map_or_else(|| ip.destination(), |edit| edit.ipv6(ip).1);
                (ip.source().into(), destination.into())
            }
        };
        TransitPath::lookup(ports, routes, addresses, now)
    }

    pub(super) fn transmit(
        self,
        ports: &mut Ports<'_>,
        path: TransitPath,
        (offset, rewrite): (usize, Option<Rewrite>),
        now: Instant,
        budget: &mut usize,
    ) -> SendResult {
        match (self, path) {
            (Self::Ipv4(ip), TransitPath::Ipv4(route)) => {
                ipv4::send(ports, route, ip, (offset, rewrite), now, budget)
            }
            (Self::Ipv6(ip), TransitPath::Ipv6(route)) => {
                ipv6::send(ports, route, ip, rewrite, now, budget)
            }
            _ => unreachable!("transit path belongs to the same datagram"),
        }
    }

    pub(super) fn forward(
        self,
        ports: &mut Ports<'_>,
        routes: Routes<'_>,
        (rewrite, path): (Option<Rewrite>, Option<TransitPath>),
        now: Instant,
        budget: &mut usize,
    ) -> SendResult {
        match self.select(ports, routes, (rewrite, path), now) {
            Ok(path) => self.transmit(ports, path, (0, rewrite), now, budget),
            Err(result) => result,
        }
    }
}
