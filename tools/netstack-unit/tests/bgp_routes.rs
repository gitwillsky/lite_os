//! 用实际 Session 的接收决定保护候选替换/撤回与共享引用生命周期；格式/驻留用例不拥有接收表。
use ::bgp::{
    AdjRibIn, AttributePool, Candidate, Effect, Handling, Input, LocRib, LocRibError, NextHop,
    OutOfMemory, Peer, PeerKind, Reachability, RoutingDomain, RoutingError, SelectedRoute,
    SelectionContext, Selector, SourceError, SourcePeer,
};
use core::{net::IpAddr, num::NonZeroU32};
use fallible_tree::FallibleMap;
use packet_wire::bgp::{
    attributes::{Attribute, MpReach, MpUnreach},
    nlri::{Family, Nlri, Prefix},
};
use router_protocol::bgp::RouteDecision;
mod bgp_peer;
use bgp_peer::{attributes, base, established, receive, update};

fn prefix(address: &str, length: u8) -> Prefix {
    Prefix {
        address: address.parse().unwrap(),
        length,
    }
}

#[test]
fn add_path_withdraws_only_the_named_path_and_reselects() {
    let config = ::bgp::Config {
        receive_add_path: true,
        ..bgp_peer::config(false)
    };
    let mut session =
        bgp_peer::established_with_capabilities(config, true, &[69, 8, 0, 1, 1, 2, 0, 2, 1, 3]);
    assert_eq!(session.negotiated().unwrap().receive_add_path, [true; 2]);
    let peer = Peer {
        id: 1,
        asn: config.peer_as,
        kind: PeerKind::External,
        address: "192.0.2.2".parse().unwrap(),
        identifier: "192.0.2.2".parse().unwrap(),
        route_reflector_client: false,
    };
    let context = SelectionContext::new(config.local_as, config.identifier, None, None).unwrap();
    let mut domain = RoutingDomain::try_new(context, [false; 2]).unwrap();
    domain
        .add_peer(
            peer,
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    let target = prefix("203.0.113.0", 24);
    let primary = [0, 0, 0, 0, 24, 203, 0, 113];
    let alternate = [0, 0, 0, 1, 24, 203, 0, 113];
    let mut fields = base(false, true);
    let first = update(&[], &attributes(&fields), &primary);
    fields
        .iter_mut()
        .find(|attribute| attribute.code == 3)
        .unwrap()
        .data = &[192, 0, 2, 3];
    let second = update(&[], &attributes(&fields), &alternate);
    for frame in [&first, &second] {
        domain
            .receive(peer.id, &receive(&mut session, frame))
            .unwrap();
    }
    let choose = |domain: &mut RoutingDomain| {
        domain
            .prepare(Family::Ipv4, |_, _, _, route| {
                let primary = route.next_hop().address == core::net::Ipv4Addr::new(192, 0, 2, 2);
                Ok((
                    route.next_hop(),
                    Some(100),
                    Reachability::Resolved {
                        interior_cost: Some(if primary { 10 } else { 20 }),
                    },
                    route.attributes(),
                ))
            })
            .unwrap()
            .commit()
    };
    let paths = |domain: &RoutingDomain| {
        domain
            .peer_routes(peer.id)
            .unwrap()
            .paths(target)
            .map(|(id, route)| (id, route.decision()))
            .collect::<Vec<_>>()
    };
    assert_eq!(choose(&mut domain), [target]);
    assert_eq!(
        paths(&domain),
        [
            (Some(0), RouteDecision::Selected),
            (Some(1), RouteDecision::NotBest),
        ]
    );
    let unknown = update(&[0, 0, 0, 2, 24, 203, 0, 113], &[], &[]);
    domain
        .receive(peer.id, &receive(&mut session, &unknown))
        .unwrap();
    assert!(domain.pending().is_empty());
    let withdrawal = update(&primary, &[], &[]);
    domain
        .receive(peer.id, &receive(&mut session, &withdrawal))
        .unwrap();
    assert_eq!(choose(&mut domain), [target]);
    assert_eq!(
        domain.selected().get(target).unwrap().next_hop().address,
        "192.0.2.3".parse::<IpAddr>().unwrap()
    );
    assert_eq!(paths(&domain), [(Some(1), RouteDecision::Selected)]);

    // 独立的 IPv6 MP 属性向量也携带编号 0；撤回只消费对应的路径。
    let reach = [
        0, 2, 1, 16, 0x20, 1, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 32,
        0x20, 1, 0x0d, 0xb8,
    ];
    let unreach = [0, 2, 1, 0, 0, 0, 0, 32, 0x20, 1, 0x0d, 0xb8];
    let target = prefix("2001:db8::", 32);
    fields.truncate(2);
    for (code, data) in [(14, reach.as_slice()), (15, unreach.as_slice())] {
        fields.push(Attribute {
            flags: 0x80,
            code,
            data,
        });
        let frame = update(&[], &attributes(&fields), &[]);
        domain
            .receive(peer.id, &receive(&mut session, &frame))
            .unwrap();
        let table = domain.peer_routes(peer.id).unwrap();
        assert_eq!(table.get(target, Some(0)).is_some(), code == 14);
        fields.clear();
    }
}

#[test]
fn dual_stack_batches_share_attributes_deduplicate_prefixes_and_report_only_effective_changes() {
    let mut pool = AttributePool::try_new().unwrap();
    let mut rib = AdjRibIn::new();
    let mut session = established(false, true);
    let ipv6 = [
        64, 0x20, 1, 0x0d, 0xb8, 0, 1, 0, 0, 64, 0x20, 1, 0x0d, 0xb8, 0, 2, 0, 0,
    ];
    let next_hop = "2001:db8::1"
        .parse::<core::net::Ipv6Addr>()
        .unwrap()
        .octets();
    let mut mp = [0; 64];
    let size = MpReach::emit(
        &mut mp,
        1,
        &next_hop,
        Nlri::parse(&ipv6, Family::Ipv6).unwrap(),
    )
    .unwrap();
    let mut list = base(false, true);
    list.extend([
        Attribute {
            flags: 0xc0,
            code: 8,
            data: &[0, 0, 0, 42],
        },
        Attribute {
            flags: 0xc0,
            code: 16,
            data: &[0, 2, 0, 1, 0, 0, 0, 9],
        },
        Attribute {
            flags: 0xc0,
            code: 32,
            data: &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3],
        },
    ]);
    list.push(Attribute {
        flags: 0x80,
        code: 14,
        data: &mp[..size],
    });
    let bytes = update(
        &[],
        &attributes(&list),
        &[24, 203, 0, 113, 24, 198, 51, 100, 24, 203, 0, 113],
    );
    let received = receive(&mut session, &bytes);
    let expected = [
        prefix("198.51.100.0", 24),
        prefix("203.0.113.0", 24),
        prefix("2001:db8:1::", 64),
        prefix("2001:db8:2::", 64),
    ];
    assert_eq!(rib.apply(&mut pool, &received).unwrap(), expected);
    assert_eq!(rib.len(), 4);
    assert_eq!(pool.len(), 1);
    assert_eq!(pool.path_count(), 1);
    assert!(rib.iter().map(|(prefix, _, _)| prefix).eq(expected));
    assert_eq!(
        rib.get(expected[0], None).unwrap().next_hop().address,
        "192.0.2.2".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        rib.get(expected[2], None).unwrap().next_hop().address,
        "2001:db8::1".parse::<IpAddr>().unwrap()
    );
    assert!(
        rib.iter()
            .all(|(_, _, route)| route.attributes().path().contains(65001))
    );
    // 通过真实接收/驻留属性执行入口策略，保护三类团体、AS4 条件、first-match、degree 0 和原始表不变。
    let configured = router_protocol::bgp::Configuration::parse(br#"{
        "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
            "address":"192.0.2.2","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":false,
            "import":{"default":"deny","preference":100,"rules":[{
                "prefix":{"network":"198.51.100.0","length":24},"as_path_contains":[65001],"communities":[42],
                "extended_communities":[[0,2,0,1,0,0,0,9]],"large_communities":[[0,0,0,1,0,0,0,2,0,0,0,3]],
                "action":{"kind":"permit","preference":0,"next_hop":"192.0.2.8"}
            }]}
        }}
    }"#).unwrap();
    let mut policy = configured
        .neighbors
        .into_values()
        .next()
        .unwrap()
        .import
        .unwrap();
    let peer = Peer {
        route_reflector_client: false,
        id: 1,
        address: "192.0.2.2".parse().unwrap(),
        identifier: "192.0.2.2".parse().unwrap(),
        asn: 65001,
        kind: PeerKind::External,
    };
    let route = rib.get(expected[0], None).unwrap();
    assert_eq!(::bgp::import_route(None, expected[0], &peer, route), None);
    let classified = ::bgp::import_route(Some(&policy), expected[0], &peer, route).unwrap();
    assert_eq!(
        (classified.next_hop.address, classified.preference),
        ("192.0.2.8".parse::<IpAddr>().unwrap(), 0)
    );
    assert!(
        ::bgp::import_route(
            Some(&policy),
            expected[1],
            &peer,
            rib.get(expected[1], None).unwrap()
        )
        .is_none()
    );
    policy.rules[0].communities.push(999);
    assert!(::bgp::import_route(Some(&policy), expected[0], &peer, route).is_none());
    policy.rules[0].communities.pop();
    let mut rejected = policy.rules[0].clone();
    rejected.action = router_protocol::bgp::RouteAction::Deny;
    policy.rules.insert(0, rejected);
    assert!(::bgp::import_route(Some(&policy), expected[0], &peer, route).is_none());
    assert_eq!(
        route.next_hop().address,
        "192.0.2.2".parse::<IpAddr>().unwrap()
    );
    assert_eq!(route.attributes().communities(), &[42]);
    assert!(rib.apply(&mut pool, &received).unwrap().is_empty());
    let mut unreachable = [0; 32];
    let size = MpUnreach::emit(
        &mut unreachable,
        1,
        Nlri::parse(&ipv6[..9], Family::Ipv6).unwrap(),
    )
    .unwrap();
    let bytes = update(
        &[24, 203, 0, 113],
        &attributes(&[Attribute {
            flags: 0x80,
            code: 15,
            data: &unreachable[..size],
        }]),
        &[],
    );
    let received = receive(&mut session, &bytes);
    assert_eq!(
        rib.apply(&mut pool, &received).unwrap(),
        [expected[1], expected[2]]
    );
    assert_eq!(rib.len(), 2);
    assert!(rib.get(expected[1], None).is_none());
    assert!(rib.get(expected[3], None).is_some());
    let bytes = update(&[], &[], &[]);
    assert!(
        rib.apply(&mut pool, &receive(&mut session, &bytes))
            .unwrap()
            .is_empty()
    );
    assert_eq!(rib.len(), 2);
}

#[test]
fn replacement_wins_over_same_message_withdrawal_and_treat_as_withdraw_removes_previous_routes() {
    let mut pool = AttributePool::try_new().unwrap();
    let mut rib = AdjRibIn::new();
    let mut session = established(false, true);
    let bytes = update(
        &[],
        &attributes(&base(false, true)),
        &[24, 203, 0, 113, 24, 198, 51, 100],
    );
    rib.apply(&mut pool, &receive(&mut session, &bytes))
        .unwrap();
    let mut list = base(false, true);
    list[2].data = &[192, 0, 2, 9];
    list.push(Attribute {
        flags: 0x80,
        code: 4,
        data: &[0, 0, 0, 7],
    });
    let bytes = update(
        &[24, 203, 0, 113, 24, 192, 0, 2],
        &attributes(&list),
        &[24, 203, 0, 113, 24, 203, 0, 113],
    );
    assert_eq!(
        rib.apply(&mut pool, &receive(&mut session, &bytes))
            .unwrap(),
        [prefix("203.0.113.0", 24)]
    );
    assert_eq!(rib.len(), 2);
    let changed = rib.get(prefix("203.0.113.0", 24), None).unwrap();
    assert_eq!(changed.attributes().med(), Some(7));
    assert_eq!(
        changed.next_hop().address,
        "192.0.2.9".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        rib.get(prefix("198.51.100.0", 24), None)
            .unwrap()
            .attributes()
            .med(),
        None
    );
    assert_eq!(pool.len(), 2);
    assert_eq!(pool.path_count(), 1);
    list[0].data = &[9];
    let bytes = update(
        &[],
        &attributes(&list),
        &[24, 203, 0, 113, 24, 198, 51, 100],
    );
    let received = receive(&mut session, &bytes);
    assert_eq!(received.handling(), Handling::TreatAsWithdraw);
    assert_eq!(
        rib.apply(&mut pool, &received).unwrap(),
        [prefix("198.51.100.0", 24), prefix("203.0.113.0", 24)]
    );
    assert!(rib.is_empty());
    assert!(pool.is_empty());
    assert_eq!(pool.path_count(), 0);
    let bytes = update(&[24, 192, 0, 2], &[], &[]);
    assert!(
        rib.apply(&mut pool, &receive(&mut session, &bytes))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn neighbor_retirement_is_isolated_and_keeps_shared_attributes_until_last_owner_releases_them() {
    let mut pool = AttributePool::try_new().unwrap();
    let mut first = AdjRibIn::new();
    let mut second = AdjRibIn::new();
    let mut a = established(false, true);
    let mut b = established(false, true);
    let bytes = update(&[], &attributes(&base(false, true)), &[0, 32, 0, 0, 0, 0]);
    first.apply(&mut pool, &receive(&mut a, &bytes)).unwrap();
    second.apply(&mut pool, &receive(&mut b, &bytes)).unwrap();
    let local = a.config();
    let context = SelectionContext::new(local.local_as, local.identifier, None, None).unwrap();
    let a_open = a.negotiated().unwrap();
    let b_open = b.negotiated().unwrap();
    let a_peer = Peer {
        route_reflector_client: false,
        id: 1,
        address: "192.0.2.2".parse().unwrap(),
        identifier: a_open.peer_identifier,
        asn: a_open.peer_as,
        kind: PeerKind::External,
    };
    let b_peer = Peer {
        route_reflector_client: false,
        id: 2,
        address: "192.0.2.3".parse().unwrap(),
        identifier: b_open.peer_identifier,
        asn: b_open.peer_as,
        kind: PeerKind::External,
    };
    let mut selector = Selector::new();
    // 政策与解析为受控输入；选择借用的是真正接收表中的属性，不建立另一份路由/属性数组。
    let reachability = Reachability::Resolved {
        interior_cost: Some(0),
    };
    assert_eq!(
        selector
            .select(
                context,
                &[
                    Candidate {
                        path_id: None,
                        peer: &a_peer,
                        next_hop: first.get(prefix("0.0.0.0", 0), None).unwrap().next_hop(),
                        attributes: first.get(prefix("0.0.0.0", 0), None).unwrap().attributes(),
                        received_path: first.get(prefix("0.0.0.0", 0), None).unwrap().attributes().path(),
                        preference: Some(100),
                        reachability
                    },
                    Candidate {
                        path_id: None,
                        peer: &b_peer,
                        next_hop: second.get(prefix("0.0.0.0", 0), None).unwrap().next_hop(),
                        attributes: second.get(prefix("0.0.0.0", 0), None).unwrap().attributes(),
                        received_path: second.get(prefix("0.0.0.0", 0), None).unwrap().attributes().path(),
                        preference: Some(100),
                        reachability
                    },
                ]
            )
            .unwrap(),
        Some(0)
    );
    assert_eq!(pool.len(), 1);
    assert_eq!(first.len(), 2);
    assert!(a.on(5, Input::Stop).into_iter().any(|effect| matches!(
        effect,
        Effect::Close {
            withdraw_routes: true,
            ..
        }
    )));
    let retired = first.retire();
    assert!(first.is_empty());
    assert_eq!(
        selector
            .select(
                context,
                &[Candidate {
                    path_id: None,
                    peer: &b_peer,
                    next_hop: second.get(prefix("0.0.0.0", 0), None).unwrap().next_hop(),
                    attributes: second.get(prefix("0.0.0.0", 0), None).unwrap().attributes(),
                    received_path: second.get(prefix("0.0.0.0", 0), None).unwrap().attributes().path(),
                    preference: Some(100),
                    reachability,
                }]
            )
            .unwrap(),
        Some(0)
    );
    assert_eq!(retired.len(), 2);
    assert!(retired.get(prefix("0.0.0.0", 0), None).is_some());
    assert!(retired.get(prefix("0.0.0.0", 32), None).is_some());
    drop(retired);
    assert_eq!(second.len(), 2);
    assert_eq!(pool.len(), 1);
    let retired = second.retire();
    assert_eq!(selector.select(context, &[]).unwrap(), None);
    let retained = retired.iter().next().unwrap().2.attributes().clone();
    drop(retired);
    assert!(second.is_empty());
    assert_eq!(pool.len(), 1);
    assert!(retained.path().contains(65001));
    drop(retained);
    assert!(pool.is_empty());
    assert_eq!(pool.path_count(), 0);
    // 域 owner 须持有取消/失败后的工作，并自动完成跨邻居重选；单独接收表/选择器无法保护该次序。
    let mut a = established(false, true);
    let mut b = established(false, true);
    let mut domain = RoutingDomain::try_new(context, [false; 2]).unwrap();
    let minimum = NonZeroU32::new(20).unwrap();
    for peer in [a_peer, b_peer] {
        domain
            .add_peer(
                peer,
                SourcePeer {
                    preference: minimum,
                    next_hop_interface: None,
                },
            )
            .unwrap();
    }
    assert_eq!(
        domain.add_peer(
            a_peer,
            SourcePeer {
                preference: minimum,
                next_hop_interface: None
            }
        ),
        Err(RoutingError::DuplicatePeer)
    );
    let raw = receive(&mut a, &bytes);
    domain.receive(a_peer.id, &raw).unwrap();
    domain.receive(b_peer.id, &receive(&mut b, &bytes)).unwrap();
    assert_eq!(
        domain.pending().len(),
        4,
        "messages coalesce before one source publication"
    );
    assert_eq!(
        domain.add_peer(
            Peer { id: 3, ..a_peer },
            SourcePeer {
                preference: minimum,
                next_hop_interface: None
            }
        ),
        Ok(())
    );
    domain.remove_peer(3).unwrap();
    fn classify<'a>(_: Prefix, _: &Peer, _: Option<u32>, received: &'a ::bgp::ReceivedRoute) -> Result<(NextHop, Option<u32>, Reachability, &'a ::bgp::RouteAttributes), OutOfMemory> {
        Ok((received.next_hop(), Some(100), Reachability::Resolved { interior_cost: Some(0) }, received.attributes()))
    }
    let prepared = domain.prepare(Family::Ipv4, classify).unwrap();
    assert_eq!(prepared.changed_prefixes().len(), 2);
    drop(prepared);
    assert_eq!(domain.pending().len(), 2);
    assert!(domain.selected().is_empty());
    // 取消/资源失败不能提前发布判定；来源确认前，原始候选仍为 Pending。
    assert!(domain.peers().all(|(_, routes)| routes.iter().all(|(_, _, route)| route.decision() == RouteDecision::Pending)));
    assert!(matches!(
        domain.prepare(Family::Ipv4, |_, _, _, _| Err(OutOfMemory)),
        Err(RoutingError::NoMemory)
    ));
    assert_eq!(domain.pending().len(), 2);
    let prepared = domain.prepare(Family::Ipv4, classify).unwrap();
    assert_eq!(
        prepared.encode_source(NonZeroU32::new(21).unwrap()),
        Err(SourceError::Preference)
    );
    drop(prepared);
    let prepared = domain.prepare(Family::Ipv4, classify).unwrap();
    prepared.encode_source(minimum).unwrap();
    prepared.commit();
    assert!(domain.pending().is_empty());
    assert_eq!(
        domain.selected().get(prefix("0.0.0.0", 0)).unwrap().peer(),
        a_peer.id
    );
    assert!(domain.peer_routes(a_peer.id).unwrap().iter().all(|(_, _, route)| route.decision() == RouteDecision::Selected));
    assert!(domain.peer_routes(b_peer.id).unwrap().iter().all(|(_, _, route)| route.decision() == RouteDecision::NotBest));
    domain.receive(b_peer.id, &receive(&mut b, &bytes)).unwrap();
    assert!(domain.pending().is_empty(), "identical UPDATE must ignore observational metadata");
    let unchanged = domain.prepare(Family::Ipv4, classify).unwrap();
    assert!(unchanged.changed_prefixes().is_empty());
    unchanged.commit();
    domain.remove_peer(a_peer.id).unwrap();
    assert_eq!(
        domain.add_peer(
            a_peer,
            SourcePeer {
                preference: minimum,
                next_hop_interface: None
            }
        ),
        Err(RoutingError::Busy)
    );
    assert_eq!(domain.peers().count(), 1);
    assert_eq!(
        domain.selected().get(prefix("0.0.0.0", 0)).unwrap().peer(),
        a_peer.id,
        "selected owner survives until withdrawal confirmation"
    );
    domain.prepare(Family::Ipv4, classify).unwrap().commit();
    assert_eq!(
        domain.selected().get(prefix("0.0.0.0", 0)).unwrap().peer(),
        b_peer.id
    );
    let prefixes = [prefix("0.0.0.0", 0), prefix("0.0.0.0", 32)];
    assert_eq!(
        domain.reconsider([prefix("192.0.2.1", 24)]),
        Err(RoutingError::Candidate(LocRibError::InvalidPrefix))
    );
    assert!(domain.pending().is_empty());
    domain.reconsider(prefixes).unwrap();
    let rejected = domain
        .prepare(Family::Ipv4, |_, _, _, received| {
            Ok((received.next_hop(), None, Reachability::Unresolved, received.attributes()))
        })
        .unwrap();
    let encoded = rejected.encode_source(minimum).unwrap();
    assert_eq!(
        router_protocol::routing::Batch::parse(&encoded)
            .unwrap()
            .routes()
            .len(),
        0
    );
    rejected.commit();
    assert!(domain.selected().is_empty());
    assert!(domain.peer_routes(b_peer.id).unwrap().iter().all(|(_, _, route)| route.decision() == RouteDecision::ImportPolicy));
    domain.reconsider(prefixes).unwrap();
    domain.prepare(Family::Ipv4, |_, _, _, received| Ok((received.next_hop(), Some(100), Reachability::Unresolved, received.attributes()))).unwrap().commit();
    assert!(domain.peer_routes(b_peer.id).unwrap().iter().all(|(_, _, route)| route.decision() == RouteDecision::Unresolved));

    assert_eq!(
        domain.peers().next().unwrap().1.len(),
        2,
        "policy rejection must keep raw routes for later re-evaluation"
    );
    domain.reconsider(prefixes).unwrap();
    domain.prepare(Family::Ipv4, classify).unwrap().commit();
    assert_eq!(domain.selected().len(), 2);
    domain.remove_peer(b_peer.id).unwrap();
    domain.prepare(Family::Ipv4, classify).unwrap().commit();
    assert!(domain.selected().is_empty() && domain.pending().is_empty());
    // 同一 UPDATE 两族分别确认；任一族取消不能消费另一族或让新消息覆盖尚未发表的工作。
    domain
        .add_peer(
            a_peer,
            SourcePeer {
                preference: minimum,
                next_hop_interface: None,
            },
        )
        .unwrap();
    let mut mp = [0; 64];
    let address = "2001:db8::1"
        .parse::<core::net::Ipv6Addr>()
        .unwrap()
        .octets();
    let size = MpReach::emit(
        &mut mp,
        1,
        &address,
        Nlri::parse(&[32, 0x20, 1, 0x0d, 0xb8], Family::Ipv6).unwrap(),
    )
    .unwrap();
    let mut list = base(false, true);
    list.push(Attribute {
        flags: 0x80,
        code: 14,
        data: &mp[..size],
    });
    let bytes = update(&[], &attributes(&list), &[24, 198, 51, 100]);
    let received = receive(&mut a, &bytes);
    domain.receive(a_peer.id, &received).unwrap();
    assert_eq!(domain.pending().len(), 2);
    domain.prepare(Family::Ipv4, classify).unwrap().commit();
    assert_eq!(domain.pending(), [prefix("2001:db8::", 32)]);
    domain.receive(a_peer.id, &received).unwrap();
    assert_eq!(domain.pending(), [prefix("2001:db8::", 32)]);
    let ipv6 = domain.prepare(Family::Ipv6, classify).unwrap();
    let encoded = ipv6.encode_source(minimum).unwrap();
    let batch = router_protocol::routing::Batch::parse(&encoded).unwrap();
    assert_eq!(batch.family(), router_protocol::routing::Family::Ipv6);
    assert_eq!(batch.routes().len(), 1);
    drop(ipv6);
    assert_eq!(domain.pending().len(), 1);
    domain.prepare(Family::Ipv6, classify).unwrap().commit();
    assert!(domain.pending().is_empty());
    domain.remove_peer(a_peer.id).unwrap();
    for family in [Family::Ipv4, Family::Ipv6] {
        domain.prepare(family, classify).unwrap().commit();
    }
    assert!(domain.selected().is_empty() && domain.pending().is_empty());
}

#[test]
fn loc_rib_owns_selected_routes_and_prepares_whole_batch_before_commit_or_cancel() {
    let mut pool = AttributePool::try_new().unwrap();
    let mut adj = AdjRibIn::new();
    let mut session = established(false, true);
    let bytes = update(
        &[],
        &attributes(&base(false, true)),
        &[24, 198, 51, 100, 24, 203, 0, 113],
    );
    adj.apply(&mut pool, &receive(&mut session, &bytes))
        .unwrap();
    let config = session.config();
    let open = session.negotiated().unwrap();
    let context = SelectionContext::new(config.local_as, config.identifier, None, None).unwrap();
    let peer = Peer {
        route_reflector_client: false,
        id: 1,
        address: "192.0.2.2".parse().unwrap(),
        identifier: open.peer_identifier,
        asn: open.peer_as,
        kind: PeerKind::External,
    };
    let backup = Peer {
        id: 2,
        address: "192.0.2.3".parse().unwrap(),
        ..peer
    };
    let first_prefix = prefix("198.51.100.0", 24);
    let second_prefix = prefix("203.0.113.0", 24);
    let ipv6_prefix = prefix("2001:db8::", 32);
    let default = prefix("0.0.0.0", 0);
    let received = adj.get(first_prefix, None).unwrap();
    let candidate = Candidate {
        path_id: None,
        peer: &peer,
        attributes: received.attributes(),
        received_path: received.attributes().path(),
        next_hop: received.next_hop(),
        preference: Some(200),
        reachability: Reachability::Resolved {
            interior_cost: Some(0),
        },
    };
    let mut selector = Selector::new();
    let selected = SelectedRoute::select(&mut selector, context, &[candidate])
        .unwrap()
        .unwrap();
    let alternate = SelectedRoute::select(
        &mut selector,
        context,
        &[Candidate {
            path_id: None,
            peer: &backup,
            ..candidate
        }],
    )
    .unwrap()
    .unwrap();
    // 属性不含 NLRI/下一跳；IPv6 后政策投影同样复用其路径/团体，不用错误的 IPv4 目标占位。
    let ipv6_hop = NextHop {
        address: "2001:db8::1".parse().unwrap(),
        link_local: Some("fe80::2".parse().unwrap()),
    };
    let ipv6 = SelectedRoute::select(
        &mut selector,
        context,
        &[Candidate {
            next_hop: ipv6_hop,
            ..candidate
        }],
    )
    .unwrap()
    .unwrap();
    assert_eq!(selected.preference(), 200);
    assert_eq!(selected.next_hop(), received.next_hop());
    let mut loc = LocRib::new();
    let initial = [first_prefix, second_prefix];
    let prepared = loc
        .prepare(
            Family::Ipv4,
            [
                Ok((second_prefix, Some(selected.clone()))),
                Ok((first_prefix, Some(selected.clone()))),
            ],
        )
        .unwrap();
    assert_eq!(prepared.changed_prefixes(), initial);
    assert!(prepared.iter().map(|(prefix, _)| prefix).eq(initial));
    assert_eq!(prepared.family(), Family::Ipv4);
    let minimum = NonZeroU32::new(20).unwrap();
    let mut source_peers = FallibleMap::new();
    assert_eq!(
        prepared.encode_source(minimum, &source_peers),
        Err(SourceError::MissingPeer)
    );
    source_peers
        .try_insert(
            peer.id,
            SourcePeer {
                preference: minimum,
                next_hop_interface: None,
            },
        )
        .unwrap();
    assert_eq!(
        prepared.encode_source(NonZeroU32::new(21).unwrap(), &source_peers),
        Err(SourceError::Preference)
    );
    let encoded = prepared.encode_source(minimum, &source_peers).unwrap();
    let source = router_protocol::routing::Batch::parse(&encoded).unwrap();
    assert_eq!(source.attributes().len(), 1);
    assert_eq!(
        source.attributes().next().unwrap(),
        router_protocol::routing::Attributes {
            preference: 20,
            metric: 0
        }
    );
    assert_eq!(source.next_hops().len(), 1);
    assert_eq!(source.routes().len(), 2);
    assert!(
        source
            .routes()
            .all(|route| route.attributes == 0 && route.next_hop == 0)
    );
    assert_eq!(
        source
            .next_hops()
            .next()
            .unwrap()
            .members()
            .next()
            .unwrap()
            .target,
        router_protocol::routing::Target::Recursive(received.next_hop().address)
    );
    assert_eq!(prepared.commit(), initial);
    let prepared = loc
        .prepare(Family::Ipv6, [Ok((ipv6_prefix, Some(ipv6.clone())))])
        .unwrap();
    assert!(prepared.iter().map(|(prefix, _)| prefix).eq([ipv6_prefix]));
    let encoded = prepared.encode_source(minimum, &source_peers).unwrap();
    let source = router_protocol::routing::Batch::parse(&encoded).unwrap();
    assert_eq!(source.family(), router_protocol::routing::Family::Ipv6);
    assert_eq!(
        source.routes().len(),
        1,
        "must not include the other family"
    );
    assert_eq!(
        source
            .next_hops()
            .next()
            .unwrap()
            .members()
            .next()
            .unwrap()
            .target,
        router_protocol::routing::Target::Recursive(ipv6_hop.address)
    );
    source_peers.get_mut(&peer.id).unwrap().next_hop_interface = NonZeroU32::new(7);
    let scoped = prepared.encode_source(minimum, &source_peers).unwrap();
    let scoped = router_protocol::routing::Batch::parse(&scoped).unwrap();
    assert_eq!(
        scoped
            .next_hops()
            .next()
            .unwrap()
            .members()
            .next()
            .unwrap()
            .target,
        router_protocol::routing::Target::Direct {
            interface_index: 7,
            gateway: Some("fe80::2".parse().unwrap()),
            on_link: true
        }
    );
    source_peers.get_mut(&peer.id).unwrap().next_hop_interface = None;
    prepared.commit();
    assert_eq!(loc.len(), 3);
    assert_eq!(loc.get(first_prefix).unwrap().peer(), 1);
    let prepared = loc
        .prepare(
            Family::Ipv4,
            [
                Ok((first_prefix, None)),
                Ok((second_prefix, Some(alternate.clone()))),
                Ok((default, Some(selected.clone()))),
            ],
        )
        .unwrap();
    assert!(
        prepared
            .iter()
            .map(|(prefix, route)| (prefix, route.peer()))
            .eq([(default, 1), (second_prefix, 2)])
    );
    assert_eq!(
        prepared.encode_source(minimum, &source_peers),
        Err(SourceError::MissingPeer)
    );
    source_peers
        .try_insert(
            backup.id,
            SourcePeer {
                preference: NonZeroU32::new(30).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    let encoded = prepared.encode_source(minimum, &source_peers).unwrap();
    let source = router_protocol::routing::Batch::parse(&encoded).unwrap();
    assert!(
        source
            .attributes()
            .map(|attributes| attributes.preference)
            .eq([20, 30])
    );
    assert!(source.routes().map(|route| route.attributes).eq([0, 1]));
    assert_eq!(
        source.next_hops().len(),
        1,
        "different peer distances still share the same target"
    );
    drop(prepared); // 模拟外部来源发布失败/取消；旧表不能跟随候选视图提前改变。
    assert!(loc.get(default).is_none());
    assert_eq!(loc.get(first_prefix).unwrap().peer(), 1);
    assert_eq!(loc.get(second_prefix).unwrap().peer(), 1);
    let prepared = loc
        .prepare(
            Family::Ipv4,
            [
                Ok((first_prefix, None)),
                Ok((second_prefix, Some(alternate.clone()))),
                Ok((default, Some(selected.clone()))),
            ],
        )
        .unwrap();
    assert_eq!(prepared.commit(), [default, first_prefix, second_prefix]);
    assert_eq!(loc.get(second_prefix).unwrap().peer(), 2);
    // 同前缀最后一次决定与旧选择一致，应不产生伪撤回/变更。
    let no_op = loc
        .prepare(
            Family::Ipv4,
            [
                Ok((second_prefix, None)),
                Ok((second_prefix, Some(alternate.clone()))),
            ],
        )
        .unwrap();
    assert!(no_op.changed_prefixes().is_empty());
    assert_eq!(no_op.iter().count(), 2);
    assert!(no_op.commit().is_empty());
    assert!(matches!(
        loc.prepare(
            Family::Ipv4,
            [Ok((first_prefix, Some(selected.clone()))), Err(OutOfMemory)]
        ),
        Err(LocRibError::NoMemory)
    ));
    assert!(loc.get(first_prefix).is_none());
    assert!(matches!(
        loc.prepare(
            Family::Ipv4,
            [Ok((prefix("192.0.2.1", 24), Some(selected.clone())))]
        ),
        Err(LocRibError::InvalidPrefix)
    ));
    assert!(matches!(
        loc.prepare(Family::Ipv6, [Ok((ipv6_prefix, Some(selected.clone())))]),
        Err(LocRibError::NextHopFamily)
    ));
    assert!(matches!(
        loc.prepare(Family::Ipv4, [Ok((ipv6_prefix, Some(ipv6.clone())))]),
        Err(LocRibError::Family)
    ));
    assert_eq!(loc.get(ipv6_prefix).unwrap().next_hop(), ipv6_hop);
    let local = SelectedRoute::select(
        &mut selector,
        context,
        &[Candidate {
            next_hop: NextHop {
                address: "fe80::3".parse().unwrap(),
                link_local: None,
            },
            ..candidate
        }],
    )
    .unwrap()
    .unwrap();
    let scoped = loc
        .prepare(Family::Ipv6, [Ok((ipv6_prefix, Some(local)))])
        .unwrap();
    assert_eq!(
        scoped.encode_source(minimum, &source_peers),
        Err(SourceError::Scope)
    );
    drop(scoped);
    assert_eq!(
        loc.get(ipv6_prefix).unwrap().next_hop(),
        ipv6_hop,
        "failed encoding cannot publish candidates"
    );
    drop(adj.retire());
    drop(selected);
    drop(alternate);
    drop(ipv6);
    assert_eq!(pool.len(), 1); // Loc-RIB 仍强持属性，接收表退出不能使发布/撤回信息失效。
    assert_eq!(loc.len(), 3);
    let removed = [default, second_prefix];
    let prepared = loc
        .prepare(Family::Ipv4, removed.map(|prefix| Ok((prefix, None))))
        .unwrap();
    assert_eq!(prepared.iter().count(), 0);
    let encoded = prepared.encode_source(minimum, &source_peers).unwrap();
    let empty = router_protocol::routing::Batch::parse(&encoded).unwrap();
    assert_eq!(
        (
            empty.routes().len(),
            empty.attributes().len(),
            empty.next_hops().len()
        ),
        (0, 0, 0)
    );
    assert_eq!(prepared.commit(), removed);
    assert_eq!(loc.len(), 1);
    assert_eq!(pool.len(), 1);
    let prepared = loc
        .prepare(Family::Ipv6, [Ok((ipv6_prefix, None))])
        .unwrap();
    assert_eq!(prepared.iter().count(), 0);
    assert_eq!(prepared.commit(), [ipv6_prefix]);
    assert!(loc.is_empty());
    assert!(pool.is_empty());
    assert_eq!(pool.path_count(), 0);
}
