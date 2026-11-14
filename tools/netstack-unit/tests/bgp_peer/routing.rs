//! 出口与政策验收共用的实际会话/域夹具；只装配生产 API，不新增生产诊断分支。
use super::bgp_peer::{attributes, base, established, receive, update};
use bgp::{
    Config, ExportContext, Input, Peer, PeerKind, Reachability, RoutingDomain, SelectionContext,
    Session, SourcePeer,
};
use core::num::NonZeroU32;
use packet_wire::bgp::{
    self as wire, Frame, Kind,
    attributes::{AsnWidth, Attribute},
    nlri::Prefix,
    open::{Open, Parameter},
};
use router_protocol::{bgp::ExportPolicy, policy::Disposition};

pub(super) fn prefix() -> Prefix {
    Prefix {
        address: "198.51.100.0".parse().unwrap(),
        length: 24,
    }
}
pub(super) fn domain() -> (RoutingDomain, Session) {
    let mut domain = RoutingDomain::try_new(
        SelectionContext::new(65000, "192.0.2.1".parse().unwrap(), None, None).unwrap(), [false; 2])
    .unwrap();
    domain
        .add_peer(
            Peer {
                route_reflector_client: false,
                id: 1,
                address: "192.0.2.2".parse().unwrap(),
                identifier: "192.0.2.2".parse().unwrap(),
                asn: 65001,
                kind: PeerKind::External,
            },
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    (domain, established(false, true))
}
pub(super) fn learn(domain: &mut RoutingDomain, session: &mut Session, extra: &[Attribute<'_>]) {
    let mut list = base(false, true);
    list.extend_from_slice(extra);
    let bytes = update(&[], &attributes(&list), &[24, 198, 51, 100]);
    domain.receive(1, &receive(session, &bytes)).unwrap();
    choose(domain);
}
pub(super) fn choose(domain: &mut RoutingDomain) {
    // 这里只隔离出口行为；真实候选图/NET 证明及确认次序由既有 routing_compilation_tests 覆盖。
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, _, _, route| {
            Ok((
                route.next_hop(),
                Some(100),
                Reachability::Resolved {
                    interior_cost: None,
                },
                route.attributes(),
            ))
        })
        .unwrap()
        .commit();
}
pub(super) fn context() -> ExportContext {
    ExportContext {
        local_as: 4200000001,
        ipv6_unicast: false,
        ipv4_unicast: true,
        send_ipv4_via_ipv6: false,
        local_address: "192.0.2.9".parse().unwrap(),
        cluster_id: None,
        target: Peer {
            route_reflector_client: false,
            id: 2,
            address: "192.0.2.3".parse().unwrap(),
            identifier: "192.0.2.3".parse().unwrap(),
            asn: 65002,
            kind: PeerKind::External,
        },
        width: AsnWidth::Four,
        interval_ns: 30,
    }
}
pub(super) fn policy() -> ExportPolicy {
    ExportPolicy {
        default: Disposition::Permit,
        rules: vec![],
    }
}
pub(super) fn receiver(context: ExportContext) -> Session {
    let config = Config {
        local_as: context.target.asn,
        confederation_peer: None,
        peer_as: if context.width == AsnWidth::Four {
            context.local_as
        } else {
            23456
        },
        identifier: context.target.identifier,
        hold_seconds: 90,
        retry_seconds: 5,
        passive: true,
        ipv4_unicast: true,
        ipv6_unicast: false,
        route_refresh: true,
        receive_add_path: false,
        send_add_path: false,
        extended_messages: false,
    };
    let mut session = Session::new(config).unwrap();
    session.on(0, Input::Start);
    session.on(1, Input::Connected(::bgp::OpenOffer::Capabilities));
    let mut bytes = [0; 128];
    let capabilities = context.local_as.to_be_bytes();
    let mut content = vec![2, 0, 70, 0];
    if context.width == AsnWidth::Four {
        content.extend_from_slice(&[65, 4]);
        content.extend_from_slice(&capabilities);
    }
    let size = Open::emit(
        &mut bytes,
        u16::try_from(context.local_as).unwrap_or(23456),
        90,
        "192.0.2.9".parse().unwrap(),
        &[Parameter {
            kind: 2,
            data: &content,
        }],
    )
    .unwrap();
    session.on(
        2,
        Input::Frame(Frame::parse(&bytes[..size], false).unwrap()),
    );
    let mut keepalive = [0; 19];
    wire::emit(&mut keepalive, Kind::Keepalive, false).unwrap();
    session.on(3, Input::Frame(Frame::parse(&keepalive, false).unwrap()));
    assert_eq!(session.state(), bgp::State::Established);
    session
}
