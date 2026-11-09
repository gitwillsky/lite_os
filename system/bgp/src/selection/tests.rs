//! 生产 codec/接收/驻留得到属性，受控候选投影保护阶段顺序和 MED 非传递陷阱；不是原生 NHT/RIB 互通验收。
use super::*;
use crate::{AttributePool, Config, Negotiated};
use SegmentKind::{Sequence as Seq, Set};
use alloc::vec;
use packet_wire::bgp::{
    Frame, Message,
    attributes::{AsPath, AsnWidth, Attribute, Attributes},
    nlri::{Family, Nlri},
    update::Update,
};

fn context() -> SelectionContext {
    SelectionContext::new(
        65000,
        Ipv4Addr::new(192, 0, 2, 254),
        Some(Ipv4Addr::new(10, 0, 0, 99)),
    )
    .unwrap()
}
fn peer(id: u8, kind: PeerKind) -> Peer {
    Peer {
        id: u64::from(id),
        address: Ipv4Addr::new(192, 0, 2, id).into(),
        identifier: Ipv4Addr::new(10, 0, 0, id),
        asn: if kind == PeerKind::Internal {
            65000
        } else {
            65001
        },
        kind,
    }
}
fn attributes(
    pool: &mut AttributePool,
    peer: &Peer,
    path: &[(SegmentKind, &[u32])],
    origin: u8,
    med: Option<u32>,
    reflected: (Option<Ipv4Addr>, &[Ipv4Addr]),
) -> RouteAttributes {
    let mut path_bytes = [0; 1024];
    let path_length = AsPath::emit(
        &mut path_bytes,
        AsnWidth::Four,
        path.iter()
            .map(|(kind, asns)| (*kind, asns.iter().copied())),
    )
    .unwrap();
    let origin = [origin];
    let mut fields = vec![
        Attribute {
            flags: 0x40,
            code: 1,
            data: &origin,
        },
        Attribute {
            flags: 0x40,
            code: 2,
            data: &path_bytes[..path_length],
        },
        Attribute {
            flags: 0x40,
            code: 3,
            data: &[192, 0, 2, 1],
        },
    ];
    if peer.kind == PeerKind::Internal {
        fields.push(Attribute {
            flags: 0x40,
            code: 5,
            data: &[0, 0, 0, 100],
        });
    }
    let med_bytes = med.unwrap_or(0).to_be_bytes();
    if med.is_some() {
        fields.push(Attribute {
            flags: 0x80,
            code: 4,
            data: &med_bytes,
        });
    }
    let originator = reflected.0.unwrap_or(Ipv4Addr::UNSPECIFIED).octets();
    if reflected.0.is_some() {
        fields.push(Attribute {
            flags: 0x80,
            code: 9,
            data: &originator,
        });
    }
    let cluster_bytes = reflected
        .1
        .iter()
        .flat_map(|address| address.octets())
        .collect::<Vec<_>>();
    if !cluster_bytes.is_empty() {
        fields.push(Attribute {
            flags: 0x80,
            code: 10,
            data: &cluster_bytes,
        });
    }
    let mut encoded = [0; 2048];
    let size = Attributes::emit(&mut encoded, &fields).unwrap();
    let mut bytes = [0; 4096];
    let size = Update::emit(
        &mut bytes,
        Nlri::parse(&[], Family::Ipv4).unwrap(),
        Attributes::parse(&encoded[..size]).unwrap(),
        Nlri::parse(&[24, 203, 0, 113], Family::Ipv4).unwrap(),
        false,
    )
    .unwrap();
    let raw = &bytes[..size];
    let Message::Update(update) = Frame::parse(raw, false).unwrap().message().unwrap() else {
        panic!("UPDATE")
    };
    let config = Config {
        local_as: 65000,
        peer_as: peer.asn,
        identifier: context().identifier,
        hold_seconds: 90,
        retry_seconds: 5,
        passive: false,
        ipv4_unicast: true,
        ipv6_unicast: true,
        route_refresh: false,
        extended_messages: false,
    };
    let negotiated = Negotiated {
        peer_as: peer.asn,
        peer_identifier: peer.identifier,
        hold_seconds: 90,
        ipv4_unicast: true,
        ipv6_unicast: true,
        four_octet_asn: true,
        send_extended: false,
        receive_extended: false,
        send_refresh: false,
        receive_refresh: false,
    };
    let received = crate::received::capture(config, negotiated, update, raw).unwrap();
    pool.intern(&received).unwrap().unwrap()
}
fn candidate<'a>(
    peer: &'a Peer,
    attributes: &'a RouteAttributes,
    cost: Option<u32>,
) -> Candidate<'a> {
    Candidate {
        peer,
        attributes,
        next_hop: NextHop {
            address: peer.address,
            link_local: None,
        },
        preference: Some(100),
        reachability: Reachability::Resolved {
            interior_cost: cost,
        },
    }
}
fn winner(selector: &mut Selector, candidates: &[Candidate<'_>]) -> Option<u64> {
    selector
        .select(context(), candidates)
        .unwrap()
        .map(|index| candidates[index].peer.id)
}

#[test]
fn med_groups_break_pairwise_cycles_independently_of_input_order_and_old_workspace_contents() {
    // A 以代价胜 B，B 以代价胜 C，C 以同 AS 的 MED 胜 A；两两 fold 会产生遍历顺序相关赢家。
    let mut pool = AttributePool::try_new().unwrap();
    let peers = [
        peer(1, PeerKind::External),
        peer(2, PeerKind::External),
        peer(3, PeerKind::External),
    ];
    let a = attributes(
        &mut pool,
        &peers[0],
        &[(Seq, &[65010])],
        0,
        Some(100),
        (None, &[]),
    );
    let b = attributes(
        &mut pool,
        &peers[1],
        &[(Seq, &[65020])],
        0,
        Some(0),
        (None, &[]),
    );
    let c = attributes(
        &mut pool,
        &peers[2],
        &[(Seq, &[65010])],
        0,
        Some(0),
        (None, &[]),
    );
    let original = [
        candidate(&peers[0], &a, Some(10)),
        candidate(&peers[1], &b, Some(20)),
        candidate(&peers[2], &c, Some(30)),
    ];
    let mut selector = Selector::new();
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        assert_eq!(
            winner(&mut selector, &order.map(|index| original[index])),
            Some(2)
        );
    }
    let mut changed = original;
    changed[0].preference = Some(200);
    assert_eq!(winner(&mut selector, &changed), Some(1));
    assert_eq!(winner(&mut selector, &[]), None);
    assert_eq!(winner(&mut selector, &original), Some(2));
}

#[test]
fn preference_path_origin_and_med_precede_external_and_interior_cost() {
    let mut pool = AttributePool::try_new().unwrap();
    let external = peer(1, PeerKind::External);
    let internal = peer(2, PeerKind::Internal);
    let long = attributes(
        &mut pool,
        &internal,
        &[(Seq, &[65010, 65011])],
        2,
        Some(100),
        (None, &[]),
    );
    let short = attributes(
        &mut pool,
        &external,
        &[(Seq, &[65010])],
        0,
        Some(0),
        (None, &[]),
    );
    let internal_short = attributes(
        &mut pool,
        &internal,
        &[(Seq, &[65010])],
        0,
        Some(0),
        (None, &[]),
    );
    let mut selector = Selector::new();
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&external, &short, Some(100)),
                candidate(&internal, &internal_short, Some(0))
            ]
        ),
        Some(1)
    );
    let preferred = Candidate {
        preference: Some(200),
        ..candidate(&internal, &long, Some(100))
    };
    assert_eq!(
        winner(
            &mut selector,
            &[candidate(&external, &short, Some(0)), preferred]
        ),
        Some(2)
    );
    let set = attributes(
        &mut pool,
        &internal,
        &[(Set, &[65010, 65011, 65012])],
        2,
        Some(100),
        (None, &[]),
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&external, &long, Some(0)),
                candidate(&internal, &set, Some(100))
            ]
        ),
        Some(2)
    );
    let origin = attributes(
        &mut pool,
        &internal,
        &[(Seq, &[65010])],
        0,
        Some(100),
        (None, &[]),
    );
    let bad_origin = attributes(
        &mut pool,
        &external,
        &[(Seq, &[65010])],
        1,
        Some(0),
        (None, &[]),
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&external, &bad_origin, Some(0)),
                candidate(&internal, &origin, Some(100))
            ]
        ),
        Some(2)
    );
    // MED 缺失为零，即使更差 IGP 代价也应在同相邻 AS 内胜过较大的 MED。
    let missing = attributes(
        &mut pool,
        &internal,
        &[(Seq, &[65010])],
        0,
        None,
        (None, &[]),
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&external, &short, Some(0)),
                candidate(&internal, &missing, Some(100))
            ]
        ),
        Some(1)
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&external, &origin, Some(0)),
                candidate(&internal, &missing, Some(100))
            ]
        ),
        Some(2)
    );
    // iBGP 的前导 AS_SET 无确定邻接 AS，按本地 AS 分组，不能取排序后的集合第一项。
    let first_set = attributes(
        &mut pool,
        &internal,
        &[(Set, &[65010, 65011])],
        0,
        Some(100),
        (None, &[]),
    );
    let second_set = attributes(
        &mut pool,
        &internal,
        &[(Set, &[65020])],
        0,
        Some(0),
        (None, &[]),
    );
    let other_internal = peer(3, PeerKind::Internal);
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&internal, &first_set, Some(0)),
                candidate(&other_internal, &second_set, Some(100))
            ]
        ),
        Some(3)
    );
    let empty_high = attributes(&mut pool, &internal, &[], 0, Some(100), (None, &[]));
    let empty_missing = attributes(&mut pool, &other_internal, &[], 0, None, (None, &[]));
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&internal, &empty_high, Some(0)),
                candidate(&other_internal, &empty_missing, Some(100))
            ]
        ),
        Some(3)
    );
    let confederation = attributes(
        &mut pool,
        &internal,
        &[
            (SegmentKind::ConfederationSequence, &[65050]),
            (Seq, &[65010]),
        ],
        0,
        Some(0),
        (None, &[]),
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&internal, &confederation, Some(100)),
                candidate(&other_internal, &origin, Some(0))
            ]
        ),
        Some(2)
    );
}

#[test]
fn reachable_unknown_cost_skips_only_the_surviving_cost_stage() {
    let mut pool = AttributePool::try_new().unwrap();
    let peers = [
        peer(1, PeerKind::External),
        peer(2, PeerKind::External),
        peer(3, PeerKind::Internal),
        peer(4, PeerKind::External),
    ];
    let route = attributes(
        &mut pool,
        &peers[0],
        &[(Seq, &[65010])],
        0,
        Some(0),
        (None, &[]),
    );
    let high_med = attributes(
        &mut pool,
        &peers[0],
        &[(Seq, &[65010])],
        0,
        Some(100),
        (None, &[]),
    );
    let internal_route = attributes(
        &mut pool,
        &peers[2],
        &[(Seq, &[65010])],
        0,
        Some(0),
        (None, &[]),
    );
    let mut selector = Selector::new();
    // 未知不是零，否则会错误选择 router ID 更高的第二条。
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&peers[0], &route, Some(50)),
                candidate(&peers[1], &route, None)
            ]
        ),
        Some(1)
    );
    // 已被 eBGP 阶段移除的未知代价不能使剩下的代价比较失效。
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&peers[0], &route, Some(30)),
                candidate(&peers[1], &route, Some(20)),
                candidate(&peers[2], &internal_route, None)
            ]
        ),
        Some(2)
    );
    // 已被 MED 移除者同样不能污染后续代价阶段。
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&peers[0], &route, Some(30)),
                candidate(&peers[1], &route, Some(20)),
                candidate(&peers[3], &high_med, None)
            ]
        ),
        Some(2)
    );
}

#[test]
fn loops_and_ineligible_routes_are_excluded_before_any_preference_comparison() {
    let mut pool = AttributePool::try_new().unwrap();
    let healthy_peer = peer(9, PeerKind::Internal);
    let four_octet_peer = peer(3, PeerKind::External);
    let peer = peer(1, PeerKind::Internal);
    let good = attributes(&mut pool, &peer, &[(Seq, &[65010])], 0, None, (None, &[]));
    let as_loop = attributes(
        &mut pool,
        &peer,
        &[(Set, &[65010, 65000])],
        0,
        None,
        (None, &[]),
    );
    let originator_loop = attributes(
        &mut pool,
        &peer,
        &[(Seq, &[65010])],
        0,
        None,
        (Some(context().identifier), &[]),
    );
    let cluster_loop = attributes(
        &mut pool,
        &peer,
        &[(Seq, &[65010])],
        0,
        None,
        (None, &[context().cluster_id.unwrap()]),
    );
    let invalid_peer = Peer { asn: 0, ..peer };
    let eligible = Candidate {
        preference: Some(0),
        ..candidate(&healthy_peer, &good, Some(0))
    };
    let mut selector = Selector::new();
    for (mut rejected, reason) in [
        (
            Candidate {
                preference: None,
                ..eligible
            },
            Rejection::ImportPolicy,
        ),
        (
            Candidate {
                reachability: Reachability::Unresolved,
                ..eligible
            },
            Rejection::Unresolved,
        ),
        (candidate(&peer, &as_loop, Some(0)), Rejection::AsLoop),
        (
            candidate(&peer, &originator_loop, Some(0)),
            Rejection::OriginatorLoop,
        ),
        (
            candidate(&peer, &cluster_loop, Some(0)),
            Rejection::ClusterLoop,
        ),
        (
            Candidate {
                peer: &invalid_peer,
                ..eligible
            },
            Rejection::InvalidPeer,
        ),
    ] {
        assert_eq!(context().rejection(&rejected), Some(reason));
        if reason != Rejection::ImportPolicy {
            rejected.preference = Some(u32::MAX);
        }
        assert_eq!(
            selector.select(context(), &[rejected, eligible]).unwrap(),
            Some(1)
        );
        assert_eq!(selector.select(context(), &[rejected]).unwrap(), None);
    }
    let four_octet_loop = attributes(
        &mut pool,
        &four_octet_peer,
        &[(Set, &[70000, 65010])],
        0,
        None,
        (None, &[]),
    );
    let four_octet_context = SelectionContext::new(70000, context().identifier, None).unwrap();
    assert_eq!(
        four_octet_context.rejection(&candidate(&four_octet_peer, &four_octet_loop, Some(0))),
        Some(Rejection::AsLoop)
    );
}

#[test]
fn reflection_identifier_then_cluster_length_then_neighbor_address_break_ties() {
    let mut pool = AttributePool::try_new().unwrap();
    let a = Peer {
        identifier: Ipv4Addr::new(10, 0, 0, 9),
        ..peer(1, PeerKind::Internal)
    };
    let b = peer(2, PeerKind::Internal);
    let id = Ipv4Addr::new(10, 0, 0, 3);
    let clusters = [Ipv4Addr::new(10, 0, 0, 7), Ipv4Addr::new(10, 0, 0, 8)];
    let low_originator = attributes(
        &mut pool,
        &a,
        &[(Seq, &[65010])],
        0,
        None,
        (Some(Ipv4Addr::new(10, 0, 0, 1)), &clusters),
    );
    let high_originator = attributes(&mut pool, &b, &[(Seq, &[65010])], 0, None, (Some(id), &[]));
    let mut selector = Selector::new();
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&a, &low_originator, Some(0)),
                candidate(&b, &high_originator, Some(0))
            ]
        ),
        Some(1)
    );
    let a = Peer {
        identifier: b.identifier,
        ..a
    };
    let long_cluster = attributes(
        &mut pool,
        &a,
        &[(Seq, &[65010])],
        0,
        None,
        (Some(id), &clusters),
    );
    let short_cluster = attributes(
        &mut pool,
        &b,
        &[(Seq, &[65010])],
        0,
        None,
        (Some(id), &clusters[..1]),
    );
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&a, &long_cluster, Some(0)),
                candidate(&b, &short_cluster, Some(0))
            ]
        ),
        Some(2)
    );
    let later_address = Peer {
        address: "192.0.2.9".parse().unwrap(),
        ..a
    };
    assert_eq!(
        winner(
            &mut selector,
            &[
                candidate(&later_address, &short_cluster, Some(0)),
                candidate(&b, &short_cluster, Some(0))
            ]
        ),
        Some(2)
    );
    let link_local = "fe80::1".parse().unwrap();
    let left = Peer {
        id: 22,
        address: link_local,
        ..a
    };
    let right = Peer {
        id: 11,
        address: link_local,
        ..b
    };
    let tied = [
        candidate(&left, &short_cluster, Some(0)),
        candidate(&right, &short_cluster, Some(0)),
    ];
    assert_eq!(winner(&mut selector, &tied), Some(11));
    assert_eq!(winner(&mut selector, &[tied[1], tied[0]]), Some(11));
}
