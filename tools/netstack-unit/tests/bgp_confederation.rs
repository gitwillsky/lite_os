//! 真实 Session→接收→入口→选路→出口链路；格式用例不能保护联盟身份与传播信任边界。
mod bgp_peer;
use SegmentKind::{ConfederationSequence as Confed, Sequence as Seq};
use bgp::{
    AdjRibOut, Candidate, Config, ExportContext, Handling, Peer, PeerKind, Reachability,
    RoutingDomain, SelectionContext, Selector, SourcePeer, import_route,
};
use bgp_peer::{attributes, base, established_with, receive, update};
use core::num::NonZeroU32;
use packet_wire::{
    PacketError,
    bgp::{
        Frame, Message,
        attributes::{AsPath, AsnWidth, Attribute, SegmentKind, Value},
        nlri::{Family, Prefix},
        update::Update,
    },
};
use router_protocol::{bgp::Configuration, policy::Disposition};

const PUBLIC: u32 = 64496;
const LOCAL: u32 = 65000;
const REMOTE: u32 = 65001;

fn config(kind: PeerKind) -> Config {
    Config {
        local_as: LOCAL,
        confederation_peer: Some(kind),
        peer_as: if kind == PeerKind::Internal {
            LOCAL
        } else {
            REMOTE
        },
        identifier: "192.0.2.1".parse().unwrap(),
        hold_seconds: 90,
        retry_seconds: 5,
        passive: false,
        ipv4_unicast: true,
        ipv6_unicast: false,
        route_refresh: true,
        extended_messages: false,
    }
}
fn prefix() -> Prefix {
    Prefix {
        address: "198.51.100.0".parse().unwrap(),
        length: 24,
    }
}
fn peer(kind: PeerKind) -> Peer {
    Peer {
        id: 1,
        address: "192.0.2.2".parse().unwrap(),
        identifier: "192.0.2.2".parse().unwrap(),
        asn: config(kind).peer_as,
        kind,
        route_reflector_client: false,
    }
}
fn message(path: &[(SegmentKind, &[u32])], extra: &[Attribute<'_>]) -> Vec<u8> {
    let mut encoded = vec![0; 4096];
    let size = AsPath::emit(
        &mut encoded,
        AsnWidth::Four,
        path.iter().flat_map(|(kind, asns)| {
            asns.chunks(255)
                .map(move |chunk| (*kind, chunk.iter().copied()))
        }),
    )
    .unwrap();
    let mut list = base(false, true);
    list[1].data = &encoded[..size];
    list.extend_from_slice(extra);
    update(&[], &attributes(&list), &[24, 198, 51, 100])
}
fn domain(local: u32, kind: PeerKind) -> RoutingDomain {
    let mut domain = RoutingDomain::try_new(
        SelectionContext::new(local, config(kind).identifier, None, Some(PUBLIC)).unwrap(),
    )
    .unwrap();
    domain
        .add_peer(
            peer(kind),
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    domain
}
fn choose(domain: &mut RoutingDomain) {
    let policy = router_protocol::bgp::ImportPolicy {
        default: Disposition::Permit,
        preference: 100,
        rules: vec![],
    };
    domain
        .prepare(Family::Ipv4, |prefix, peer, route| {
            let decision = import_route(Some(&policy), prefix, peer, route).unwrap();
            Ok((
                decision.next_hop,
                Some(decision.preference),
                Reachability::Resolved {
                    interior_cost: Some(0),
                },
                route.attributes(),
            ))
        })
        .unwrap()
        .commit();
}
fn target(kind: PeerKind, width: AsnWidth) -> ExportContext {
    ExportContext {
        local_as: if kind == PeerKind::External {
            PUBLIC
        } else {
            LOCAL
        },
        ipv6_unicast: false,
        ipv4_unicast: true,
        send_ipv4_via_ipv6: false,
        local_address: "192.0.2.1".parse().unwrap(),
        cluster_id: None,
        target: Peer {
            id: 2,
            asn: match kind {
                PeerKind::Internal => LOCAL,
                PeerKind::Confederation => 65002,
                PeerKind::External => 64501,
            },
            kind,
            address: "192.0.2.3".parse().unwrap(),
            identifier: "192.0.2.3".parse().unwrap(),
            route_reflector_client: false,
        },
        width,
        interval_ns: 0,
    }
}
fn export(
    domain: &mut RoutingDomain,
    outgoing: &mut AdjRibOut,
    context: ExportContext,
) -> Option<Vec<u8>> {
    outgoing.changed(prefix(), 0).unwrap();
    outgoing
        .prepare(
            domain,
            Some(&router_protocol::bgp::ExportPolicy {
                default: Disposition::Permit,
                rules: vec![],
            }),
            context,
            0,
        )
        .unwrap()
}
fn path(message: &[u8], width: AsnWidth) -> Vec<(SegmentKind, Vec<u32>)> {
    let update = Update::parse(&message[19..]).unwrap();
    let attribute = update
        .attributes()
        .unwrap()
        .find(|attribute| attribute.code == 2)
        .unwrap();
    let Value::AsPath(path) = attribute.decode(width).unwrap() else {
        panic!("AS_PATH")
    };
    path.segments()
        .map(|segment| (segment.kind, segment.asns().collect()))
        .collect()
}

#[test]
fn configuration_selects_member_and_public_open_identity_without_numeric_role_inference() {
    assert_eq!(
        bgp_peer::established(false, true).config().peer_kind(),
        PeerKind::External
    );
    let document = br#"{"local_as":65000,"identifier":"192.0.2.1","neighbors":{},
        "confederation":{"identifier":64496,"members":[65002,65001]}}"#;
    let configured = Configuration::parse(document).unwrap();
    assert_eq!(
        Configuration::parse(&configured.encode().unwrap()).unwrap(),
        configured
    );
    let confederation = configured.confederation.as_ref().unwrap();
    assert!(
        confederation
            .members
            .iter()
            .map(|asn| asn.get())
            .eq([65001, 65002])
    );
    for (asn, kind, own) in [
        (LOCAL, PeerKind::Internal, LOCAL),
        (REMOTE, PeerKind::Confederation, LOCAL),
        (64500, PeerKind::External, PUBLIC),
    ] {
        let remote = NonZeroU32::new(asn).unwrap();
        assert_eq!(confederation.peer_kind(configured.local_as, remote), kind);
        let own_as = confederation.session_as(configured.local_as, remote);
        assert_eq!(own_as, own);
        let parameters = Config {
            local_as: own_as,
            peer_as: asn,
            confederation_peer: Some(kind),
            ..config(kind)
        };
        let mut bytes = [0; 128];
        let size = parameters
            .emit_open(bgp::OpenOffer::Capabilities, &mut bytes)
            .unwrap();
        let Message::Open(open) = Frame::parse(&bytes[..size], false)
            .unwrap()
            .message()
            .unwrap()
        else {
            panic!("OPEN")
        };
        assert_eq!(u32::from(open.asn()), own);
        let session = established_with(parameters, true);
        assert_eq!(session.config().peer_kind(), kind);
    }
    // 公开号码可与成员号码重合；AS 数值相等不能把已配置的外部邻居误作 iBGP。
    let equal = Configuration::parse(
        br#"{"local_as":64496,"identifier":"192.0.2.1",
        "neighbors":{},"confederation":{"identifier":64496,"members":[65001]}}"#,
    )
    .unwrap();
    equal.validate().unwrap();
    let outside = Config {
        local_as: PUBLIC,
        peer_as: PUBLIC,
        confederation_peer: Some(PeerKind::External),
        ..config(PeerKind::External)
    };
    assert_eq!(
        established_with(outside, true).config().peer_kind(),
        PeerKind::External
    );
    for bad in [
        br#"{"identifier":64496,"members":[65001,65001]}"#.as_slice(),
        br#"{"identifier":0,"members":[]}"#,
        br#"{"identifier":64496,"members":[0]}"#,
    ] {
        let mut document =
            br#"{"local_as":65000,"identifier":"192.0.2.1","neighbors":{},"confederation":"#
                .to_vec();
        document.extend_from_slice(bad);
        document.push(b'}');
        assert!(Configuration::parse(&document).is_err());
    }
}

#[test]
fn trusted_member_boundaries_control_acceptance_attributes_and_treat_as_withdraw() {
    let bytes = message(
        &[(Confed, &[REMOTE]), (Seq, &[64500])],
        &[
            Attribute {
                flags: 0x40,
                code: 5,
                data: &[0, 0, 0, 200],
            },
            Attribute {
                flags: 0x80,
                code: 9,
                data: &[192, 0, 2, 9],
            },
            Attribute {
                flags: 0x80,
                code: 10,
                data: &[192, 0, 2, 10],
            },
        ],
    );
    for kind in [
        PeerKind::Confederation,
        PeerKind::Internal,
        PeerKind::External,
    ] {
        let mut session = established_with(config(kind), true);
        let received = receive(&mut session, &bytes);
        if kind == PeerKind::External {
            assert_eq!(received.handling(), Handling::TreatAsWithdraw);
            assert!(
                received
                    .changes()
                    .all(|change| change == bgp::Change::Withdraw(prefix()))
            );
        } else {
            assert_eq!(received.handling(), Handling::Accept);
            assert_eq!(
                received.received_attributes().any(|attr| attr.code == 9),
                kind == PeerKind::Internal
            );
            let mut domain = domain(LOCAL, kind);
            domain.receive(1, &received).unwrap();
            let route = domain.peer_routes(1).unwrap().get(prefix()).unwrap();
            let policy = router_protocol::bgp::ImportPolicy {
                default: Disposition::Permit,
                preference: 100,
                rules: vec![],
            };
            assert_eq!(
                import_route(Some(&policy), prefix(), &peer(kind), route)
                    .unwrap()
                    .preference,
                200
            );
        }
    }
    let mut session = established_with(config(PeerKind::Confederation), true);
    let mut domain = domain(LOCAL, PeerKind::Confederation);
    domain.receive(1, &receive(&mut session, &bytes)).unwrap();
    let invalid = message(&[(Seq, &[REMOTE])], &[]);
    let received = receive(&mut session, &invalid);
    assert_eq!(received.handling(), Handling::TreatAsWithdraw);
    domain.receive(1, &received).unwrap();
    assert!(domain.peer_routes(1).unwrap().is_empty());
    let without_preference = message(&[(Confed, &[REMOTE])], &[]);
    domain
        .receive(1, &receive(&mut session, &without_preference))
        .unwrap();
    choose(&mut domain);
    assert_eq!(domain.export_route(prefix()).unwrap().preference, 100);
}

#[test]
fn publication_preserves_member_attributes_and_sanitizes_external_identity() {
    let bytes = message(
        &[(Confed, &[REMOTE]), (Seq, &[65002, 64500])],
        &[
            Attribute {
                flags: 0x40,
                code: 5,
                data: &[0, 0, 0, 200],
            },
            Attribute {
                flags: 0x80,
                code: 4,
                data: &[0, 0, 0, 17],
            },
            Attribute {
                flags: 0xc0,
                code: 16,
                data: &[0x40, 2, 0, 1, 0, 0, 0, 9],
            },
        ],
    );
    let mut session = established_with(config(PeerKind::Confederation), true);
    let mut domain = domain(LOCAL, PeerKind::Confederation);
    domain.receive(1, &receive(&mut session, &bytes)).unwrap();
    choose(&mut domain);
    for kind in [
        PeerKind::Internal,
        PeerKind::Confederation,
        PeerKind::External,
    ] {
        let context = target(kind, AsnWidth::Four);
        let sent = export(&mut domain, &mut AdjRibOut::new(), context).unwrap();
        let expected = match kind {
            PeerKind::Internal => vec![(Confed, vec![REMOTE]), (Seq, vec![65002, 64500])],
            PeerKind::Confederation => {
                vec![(Confed, vec![LOCAL, REMOTE]), (Seq, vec![65002, 64500])]
            }
            PeerKind::External => vec![(Seq, vec![PUBLIC, 65002, 64500])],
        };
        assert_eq!(path(&sent, AsnWidth::Four), expected);
        let update = Update::parse(&sent[19..]).unwrap();
        let fields = update.attributes().unwrap().collect::<Vec<_>>();
        for code in [4, 5, 16] {
            assert_eq!(
                fields.iter().any(|attr| attr.code == code),
                kind != PeerKind::External
            );
        }
        assert_eq!(
            fields.iter().find(|attr| attr.code == 3).unwrap().data,
            if kind == PeerKind::External {
                &[192, 0, 2, 1]
            } else {
                &[192, 0, 2, 2]
            }
        );
        let remote = Config {
            local_as: context.target.asn,
            peer_as: context.local_as,
            confederation_peer: Some(kind),
            ..config(kind)
        };
        assert_eq!(
            receive(&mut established_with(remote, true), &sent).handling(),
            Handling::Accept
        );
    }
    // 同类段不足 255 项时原段前置；达到上限时新开段，不能发出零长度或溢出的段。
    for count in [254, 255, 256] {
        let members = vec![REMOTE; count];
        let bytes = message(&[(Confed, &members), (Seq, &[64500])], &[]);
        domain.receive(1, &receive(&mut session, &bytes)).unwrap();
        choose(&mut domain);
        let sent = export(
            &mut domain,
            &mut AdjRibOut::new(),
            target(PeerKind::Confederation, AsnWidth::Four),
        )
        .unwrap();
        let segments = path(&sent, AsnWidth::Four);
        let counts = segments
            .iter()
            .filter(|(kind, _)| *kind == Confed)
            .map(|(_, values)| values.len())
            .collect::<Vec<_>>();
        assert_eq!(
            counts,
            if count == 254 {
                vec![255]
            } else if count == 255 {
                vec![1, 255]
            } else {
                vec![1, 255, 1]
            }
        );
        assert!(
            segments
                .iter()
                .flat_map(|(_, values)| values)
                .copied()
                .eq([LOCAL].into_iter().chain(members).chain([64500]))
        );
    }
}

#[test]
fn well_known_communities_respect_member_and_confederation_boundaries() {
    for community in [0xffff_ff01u32, 0xffff_ff02, 0xffff_ff03] {
        let value = community.to_be_bytes();
        let bytes = message(
            &[(Confed, &[REMOTE])],
            &[Attribute {
                flags: 0xc0,
                code: 8,
                data: &value,
            }],
        );
        let mut session = established_with(config(PeerKind::Confederation), true);
        let mut domain = domain(LOCAL, PeerKind::Confederation);
        domain.receive(1, &receive(&mut session, &bytes)).unwrap();
        choose(&mut domain);
        for kind in [
            PeerKind::Internal,
            PeerKind::Confederation,
            PeerKind::External,
        ] {
            let allowed = community != 0xffff_ff02
                && kind != PeerKind::External
                && (community != 0xffff_ff03 || kind == PeerKind::Internal);
            assert_eq!(
                export(
                    &mut domain,
                    &mut AdjRibOut::new(),
                    target(kind, AsnWidth::Four)
                )
                .is_some(),
                allowed
            );
        }
    }
}

#[test]
fn unmappable_confederation_withdraws_only_the_old_peer_and_does_not_leak_private_as() {
    let mut session = established_with(config(PeerKind::Confederation), true);
    let mut domain = domain(LOCAL, PeerKind::Confederation);
    let initial = message(&[(Confed, &[REMOTE]), (Seq, &[64500])], &[]);
    domain.receive(1, &receive(&mut session, &initial)).unwrap();
    choose(&mut domain);
    let mut outgoing = AdjRibOut::new();
    let old = target(PeerKind::Confederation, AsnWidth::Two);
    assert!(export(&mut domain, &mut outgoing, old).is_some());
    outgoing.sent(0);
    let changed = message(&[(Confed, &[REMOTE, 70000]), (Seq, &[80000])], &[]);
    domain.receive(1, &receive(&mut session, &changed)).unwrap();
    choose(&mut domain);
    let withdrawn = export(&mut domain, &mut outgoing, old).unwrap();
    assert_eq!(
        Update::parse(&withdrawn[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .collect::<Vec<_>>(),
        [prefix()]
    );
    assert_eq!(
        outgoing.rejected_route(),
        Some((prefix(), PacketError::Unsupported))
    );
    outgoing.sent(0);
    assert!(outgoing.is_empty());
    let external = export(
        &mut domain,
        &mut AdjRibOut::new(),
        target(PeerKind::External, AsnWidth::Two),
    )
    .unwrap();
    let mut remote = established_with(
        Config {
            local_as: 65002,
            peer_as: PUBLIC,
            confederation_peer: None,
            ..config(PeerKind::External)
        },
        false,
    );
    let received = receive(&mut remote, &external);
    assert_eq!(received.handling(), Handling::Accept);
    assert!(
        received
            .path()
            .unwrap()
            .segments()
            .flat_map(|segment| segment.asns())
            .eq([PUBLIC, 80000])
    );
}

#[test]
fn member_loops_use_confederation_segments_and_internal_routes_compete_on_igp_cost() {
    let context = SelectionContext::new(
        LOCAL,
        config(PeerKind::Internal).identifier,
        None,
        Some(PUBLIC),
    )
    .unwrap();
    let mut session = established_with(config(PeerKind::Internal), true);
    for (segments, rejected) in [
        (vec![(Confed, vec![REMOTE]), (Seq, vec![LOCAL])], false),
        (vec![(Confed, vec![LOCAL]), (Seq, vec![64500])], true),
        (vec![(Confed, vec![REMOTE]), (Seq, vec![PUBLIC])], true),
    ] {
        let segments = segments
            .iter()
            .map(|(kind, values)| (*kind, values.as_slice()))
            .collect::<Vec<_>>();
        let bytes = message(
            &segments,
            &[Attribute {
                flags: 0x40,
                code: 5,
                data: &[0, 0, 0, 100],
            }],
        );
        let mut domain = domain(LOCAL, PeerKind::Internal);
        domain.receive(1, &receive(&mut session, &bytes)).unwrap();
        let route = domain.peer_routes(1).unwrap().get(prefix()).unwrap();
        let internal = peer(PeerKind::Internal);
        let member = peer(PeerKind::Confederation);
        let candidate = |peer, cost| Candidate {
            peer,
            attributes: route.attributes(),
            received_path: route.attributes().path(),
            next_hop: route.next_hop(),
            preference: Some(100),
            reachability: Reachability::Resolved {
                interior_cost: Some(cost),
            },
        };
        let a = candidate(&internal, 1);
        assert_eq!(
            context.rejection(&a),
            rejected.then_some(bgp::Rejection::AsLoop)
        );
        if !rejected {
            assert_eq!(
                Selector::new()
                    .select(context, &[candidate(&member, 20), a])
                    .unwrap(),
                Some(1)
            );
        } else {
            // 后政策删除本地成员/公开号码也不能掩盖接收路径中的环路。
            let original = route.attributes().clone();
            let next_hop = a.next_hop;
            let preference = a.preference;
            let reachability = a.reachability;
            let excluded = [
                NonZeroU32::new(PUBLIC).unwrap(),
                NonZeroU32::new(LOCAL).unwrap(),
            ];
            let projected = domain
                .project_attributes(
                    &original,
                    bgp::AttributeChanges {
                        as_path_exclude: &excluded,
                        ..Default::default()
                    },
                )
                .unwrap();
            let edited = Candidate {
                peer: &internal,
                attributes: &projected,
                received_path: original.path(),
                next_hop,
                preference,
                reachability,
            };
            assert_eq!(context.rejection(&edited), Some(bgp::Rejection::AsLoop));
        }
    }
}
