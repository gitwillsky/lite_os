//! 原始接收、入口编辑、确认选择与实际出口报文的政策边界；与传输队列用例分开维护。
mod bgp_peer;
#[path = "bgp_peer/routing.rs"]
mod routing;
#[path = "bgp_policy/scalar.rs"]
mod scalar;
use bgp::{AdjRibOut, Peer, Reachability, SourcePeer};
use bgp_peer::{attributes, base, established, receive, update};
use core::num::NonZeroU32;
use packet_wire::bgp::{self as wire, attributes::Attribute, update::Update};
use router_protocol::bgp::RouteAction;
use routing::{choose, context, domain, learn, policy, prefix, receiver};

#[test]
fn all_community_edits_reach_the_peer_and_outbound_scope_marks_apply_to_the_recipient() {
    use bgp::{AttributePool, ExportContext, PeerKind, RoutingDomain, SelectionContext};
    let (mut domain, mut session) = domain();
    learn(
        &mut domain,
        &mut session,
        &[
            Attribute {
                flags: 0xe0,
                code: 8,
                data: &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3],
            },
            Attribute {
                flags: 0xe0,
                code: 16,
                data: &[0, 2, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 0, 0, 0, 2],
            },
            Attribute {
                flags: 0xe0,
                code: 32,
                data: &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3],
            },
        ],
    );
    let configured = router_protocol::bgp::Configuration::parse(br#"{
      "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
        "address":"192.0.2.2","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":true,
        "import":{"default":"deny","preference":100,"rules":[{"action":{"kind":"permit",
          "communities":{"replace":[3,1,2,3],"remove":[9,2,2],"add":[4,2,4]},
          "extended_communities":{"remove":[[0,2,0,0,0,0,0,1]],"add":[[0,2,0,0,0,0,0,3],[64,2,0,0,0,0,0,9]]},
          "large_communities":{"replace":[],"add":[[0,0,0,9,0,0,0,2,0,0,0,3],[0,0,0,9,0,0,0,2,0,0,0,3]]}
        }}]},
        "export":{"default":"deny","rules":[{"communities":[4],"action":{"kind":"permit",
          "communities":{"remove":[1,4],"add":[5,4294967041]},
          "extended_communities":{"add":[[0,2,0,0,0,0,0,4]]},"large_communities":{"replace":[]}
        }}]}
      }}}"#).unwrap();
    let parameters = configured.neighbors.values().next().unwrap();
    let source = *domain.peer(1).unwrap();
    let received = domain.peer_routes(1).unwrap().get(prefix(), None).unwrap();
    let original = received.attributes().clone();
    let decision =
        bgp::import_route(parameters.import.as_ref(), prefix(), &source, received).unwrap();
    let projected = domain
        .project_attributes(&original, decision.changes())
        .unwrap();
    let repeated = domain
        .project_attributes(&original, decision.changes())
        .unwrap();
    assert_eq!(projected.communities(), &[1, 2, 3, 4]);
    assert!(core::ptr::eq(
        projected.communities(),
        repeated.communities()
    ));
    assert_eq!(
        projected.extended_communities(),
        &[
            [0, 2, 0, 0, 0, 0, 0, 2],
            [0, 2, 0, 0, 0, 0, 0, 3],
            [64, 2, 0, 0, 0, 0, 0, 9]
        ]
    );
    assert_eq!(
        projected.large_communities(),
        &[[0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 0, 3]]
    );
    assert_eq!(original.communities(), &[1, 2, 3]);
    domain.reconsider([prefix()]).unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, _, _, route| {
            Ok((
                route.next_hop(),
                Some(decision.preference),
                Reachability::Resolved {
                    interior_cost: None,
                },
                &projected,
            ))
        })
        .unwrap()
        .commit();
    let mut outgoing = AdjRibOut::default();
    outgoing.changed(&domain, prefix(), 0).unwrap();
    let message = outgoing
        .prepare(&mut domain, parameters.export.as_ref(), context(), 0)
        .unwrap()
        .unwrap();
    outgoing.sent(1);
    let mut remote = receiver(context());
    let received = receive(&mut remote, &message);
    let mut pool = AttributePool::try_new().unwrap();
    let actual = pool.intern(&received).unwrap().unwrap();
    assert_eq!(actual.communities(), &[2, 3, 5, 0xffff_ff01]);
    assert_eq!(
        actual.extended_communities(),
        &[
            [0, 2, 0, 0, 0, 0, 0, 2],
            [0, 2, 0, 0, 0, 0, 0, 3],
            [0, 2, 0, 0, 0, 0, 0, 4]
        ]
    );
    assert!(actual.large_communities().is_empty());
    assert!(actual.is_partial(8));
    assert!(actual.is_partial(16));
    assert!(!actual.is_partial(32));
    assert_eq!(
        domain
            .selected()
            .get(prefix())
            .unwrap()
            .attributes()
            .large_communities(),
        projected.large_communities()
    );
    // 出口新加 NO_EXPORT 随公告送达，但收件路由器不得把它继续传到另一个外部 AS。
    let mut downstream = RoutingDomain::try_new(
        SelectionContext::new(context().target.asn, context().target.identifier, None, None).unwrap(), [false; 2])
    .unwrap();
    downstream
        .add_peer(
            Peer {
                id: 1,
                asn: context().local_as,
                address: context().local_address,
                identifier: match context().local_address { core::net::IpAddr::V4(address)=>address,_=>unreachable!("IPv4 fixture") },
                kind: PeerKind::External,
                route_reflector_client: false,
            },
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    downstream.receive(1, &received).unwrap();
    choose(&mut downstream);
    let next = ExportContext {
        local_as: context().target.asn,
        local_address: context().target.identifier.into(),
        target: Peer {
            id: 3,
            asn: 65003,
            identifier: "192.0.2.6".parse().unwrap(),
            address: "192.0.2.6".parse().unwrap(),
            ..context().target
        },
        ..context()
    };
    let mut beyond = AdjRibOut::default();
    beyond.changed(&downstream, prefix(), 0).unwrap();
    assert!(
        beyond
            .prepare(&mut downstream, Some(&policy()), next, 0)
            .unwrap()
            .is_none()
    );
    let internal = ExportContext {
        target: Peer {
            asn: next.local_as,
            kind: PeerKind::Internal,
            ..next.target
        },
        ..next
    };
    beyond.changed(&downstream, prefix(), 0).unwrap();
    assert!(
        beyond
            .prepare(&mut downstream, Some(&policy()), internal, 0)
            .unwrap()
            .is_some()
    );
    // 出口清除不能覆盖 Loc-RIB 上已有的限制，符合 FRR 的先准入再做出口编辑顺序。
    let mut rewrite = parameters.export.clone().unwrap();
    rewrite.rules[0].communities.clear();
    if let RouteAction::Permit {
        communities: Some(edit),
        ..
    } = &mut rewrite.rules[0].action
    {
        edit.replace = Some(vec![]);
        edit.add.clear();
        edit.remove.clear();
    }
    let mut blocked = AdjRibOut::default();
    blocked.changed(&downstream, prefix(), 0).unwrap();
    assert!(
        blocked
            .prepare(&mut downstream, Some(&rewrite), next, 0)
            .unwrap()
            .is_none()
    );
    // 大集合无法放入标准报文时只撤回旧公告，不能把可管理的尺寸限制升级为进程重启。
    if let RouteAction::Permit {
        communities: Some(edit),
        ..
    } = &mut rewrite.rules[0].action
    {
        edit.replace = Some((0..17000).collect());
    }
    outgoing.changed(&domain, prefix(), 31).unwrap();
    let withdrawn = outgoing
        .prepare(&mut domain, Some(&rewrite), context(), 31)
        .unwrap()
        .unwrap();
    assert_eq!(
        Update::parse(&withdrawn[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .collect::<Vec<_>>(),
        [prefix()]
    );
    assert_eq!(outgoing.rejected_route(), Some((prefix(), packet_wire::PacketError::Capacity)));
    assert_eq!(
        domain
            .peer_routes(1)
            .unwrap()
            .get(prefix(), None)
            .unwrap()
            .attributes(),
        &original
    );
    domain.reconsider([prefix()]).unwrap();
    choose(&mut domain);
    assert_eq!(
        domain
            .selected()
            .get(prefix())
            .unwrap()
            .attributes()
            .communities(),
        original.communities()
    );
}

#[test]
fn path_edits_change_selection_and_wire_without_poisoning_original_routes() {
    use packet_wire::bgp::attributes::{AsnWidth, SegmentKind};
    let (mut domain, mut session) = domain();
    learn(&mut domain, &mut session, &[]);
    let first = *domain.peer(1).unwrap();
    let second = Peer {
        id: 4,
        identifier: "192.0.2.5".parse().unwrap(),
        address: "192.0.2.5".parse().unwrap(),
        ..first
    };
    domain
        .add_peer(
            second,
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    let mut session = established(false, true);
    let bytes = update(&[], &attributes(&base(false, true)), &[24, 198, 51, 100]);
    domain
        .receive(second.id, &receive(&mut session, &bytes))
        .unwrap();
    let configuration = router_protocol::bgp::Configuration::parse(br#"{
      "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
        "address":"192.0.2.2","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":true,
        "import":{"default":"deny","preference":100,"rules":[{"as_path_contains":[65001],"as_path_regex":"^65001$","action":{"kind":"permit","as_path_prepend":[70000,70000],"as_path_exclude":[65001,65001]}}]},
        "export":{"default":"deny","rules":[{"as_path_contains":[65001],"as_path_regex":"_65001$","action":{"kind":"permit","as_path_prepend":[80000,80000],"as_path_exclude":[65001]}}]}
      }}}"#).unwrap();
    let parameters = configuration.neighbors.values().next().unwrap();
    let original = domain
        .peer_routes(first.id)
        .unwrap()
        .get(prefix(), None)
        .unwrap()
        .attributes()
        .clone();
    let decision = bgp::import_route(
        parameters.import.as_ref(),
        prefix(),
        &first,
        domain.peer_routes(first.id).unwrap().get(prefix(), None).unwrap(),
    )
    .unwrap();
    // 实际入口必须按完整 AS 边界筛选，不能把数字子串当成 ASN，编辑前后也不能混淆。
    let mut filtered = parameters.import.clone().unwrap();
    for (pattern, permitted) in [
        ("_65001_", true),
        ("^6500$", false),
        ("^70000_", false),
        ("^$", false),
    ] {
        filtered.rules[0].as_path_regex =
            Some(router_protocol::bgp::AsPathExpression::compile(pattern.into()).unwrap());
        assert_eq!(
            bgp::import_route(
                Some(&filtered),
                prefix(),
                &first,
                domain.peer_routes(first.id).unwrap().get(prefix(), None).unwrap()
            )
            .is_some(),
            permitted
        );
    }
    let edited = domain
        .project_attributes(&original, decision.changes())
        .unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, peer, _, route| {
            Ok((
                route.next_hop(),
                Some(100),
                Reachability::Resolved {
                    interior_cost: None,
                },
                if peer.id == first.id {
                    &edited
                } else {
                    route.attributes()
                },
            ))
        })
        .unwrap()
        .commit();
    assert_eq!(domain.selected().get(prefix()).unwrap().peer(), second.id);
    assert_eq!(original.path().hop_count(), 1);
    assert_eq!(edited.path().hop_count(), 2);
    // 本地 AS 被策略加入也必须进入正常去环，不得只检查编辑前的原始路径。
    let loop_replace = router_protocol::bgp::AsPathReplacement {
        from: Some(NonZeroU32::new(65001).unwrap()),
        to: NonZeroU32::new(65000).unwrap(),
    };
    let looped = domain
        .project_attributes(
            &original,
            bgp::AttributeChanges {
                as_path_replace: Some(&loop_replace),
                ..Default::default()
            },
        )
        .unwrap();
    domain.reconsider([prefix()]).unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, _, _, route| {
            Ok((
                route.next_hop(),
                Some(100),
                Reachability::Resolved {
                    interior_cost: None,
                },
                &looped,
            ))
        })
        .unwrap()
        .commit();
    assert!(domain.selected().get(prefix()).is_none());
    assert!(
        domain
            .peers()
            .all(|(_, routes)| routes.get(prefix(), None).unwrap().decision()
                == router_protocol::bgp::RouteDecision::AsLoop)
    );
    let observed = edited.observe().unwrap();
    assert!(
        observed
            .path
            .iter()
            .flat_map(|segment| segment.asns.iter().copied())
            .eq([70000, 70000])
    );
    assert_eq!(original.observe().unwrap().path[0].asns, [65001]);
    domain.reconsider([prefix()]).unwrap();
    choose(&mut domain);
    assert_eq!(domain.selected().get(prefix()).unwrap().peer(), first.id);
    for width in [AsnWidth::Two, AsnWidth::Four] {
        let target = bgp::ExportContext { width, ..context() };
        let mut outgoing = AdjRibOut::default();
        outgoing.changed(&domain, prefix(), 0).unwrap();
        let bytes = outgoing
            .prepare(&mut domain, parameters.export.as_ref(), target, 0)
            .unwrap()
            .unwrap();
        let mut session = receiver(target);
        let received = receive(&mut session, &bytes);
        let path = received.path().unwrap();
        assert!(
            path.segments()
                .all(|segment| segment.kind() == SegmentKind::Sequence)
        );
        assert!(path.segments().flat_map(|segment| segment.asns()).eq([
            target.local_as,
            80000,
            80000
        ]));
        outgoing.sent(0);
        // 同一出口允许指定/全部替换，并能清除动作恢复原路径；OLD 对端经 AS4 恢复真实 ASN。
        let mut replaced_policy = parameters.export.clone().unwrap();
        for from in [Some(NonZeroU32::new(65001).unwrap()), None] {
            let RouteAction::Permit {
                as_path_exclude,
                as_path_replace,
                ..
            } = &mut replaced_policy.rules[0].action
            else {
                unreachable!()
            };
            *as_path_exclude = None;
            *as_path_replace = Some(Box::new(router_protocol::bgp::AsPathReplacement {
                from,
                to: NonZeroU32::new(90000).unwrap(),
            }));
            outgoing.changed(&domain, prefix(), 31).unwrap();
            if let Some(bytes) = outgoing
                .prepare(&mut domain, Some(&replaced_policy), target, 31)
                .unwrap()
            {
                let received = receive(&mut session, &bytes);
                assert!(
                    received
                        .path()
                        .unwrap()
                        .segments()
                        .flat_map(|segment| segment.asns())
                        .eq([target.local_as, 80000, 80000, 90000])
                );
                outgoing.sent(31);
            } else {
                // 第二个动作语义等价，不能重复发布同一出口值。
                assert!(from.is_none());
            }
        }
        let RouteAction::Permit {
            as_path_replace, ..
        } = &mut replaced_policy.rules[0].action
        else {
            unreachable!()
        };
        *as_path_replace = None;
        outgoing.changed(&domain, prefix(), 61).unwrap();
        let bytes = outgoing
            .prepare(&mut domain, Some(&replaced_policy), target, 61)
            .unwrap()
            .unwrap();
        assert!(
            receive(&mut session, &bytes)
                .path()
                .unwrap()
                .segments()
                .flat_map(|segment| segment.asns())
                .eq([target.local_as, 80000, 80000, 65001])
        );
        outgoing.sent(61);
        let mut loop_policy = replaced_policy.clone();
        let RouteAction::Permit {
            as_path_prepend, ..
        } = &mut loop_policy.rules[0].action
        else {
            unreachable!()
        };
        *as_path_prepend = Some(Box::new([NonZeroU32::new(target.target.asn).unwrap()]));
        outgoing.changed(&domain, prefix(), 91).unwrap();
        let bytes = outgoing
            .prepare(&mut domain, Some(&loop_policy), target, 91)
            .unwrap()
            .unwrap();
        let withdrawal = Update::parse(&bytes[19..]).unwrap();
        assert_eq!(withdrawal.withdrawn().unwrap().prefixes().count(), 1);
        assert_eq!(withdrawal.announced().unwrap().prefixes().count(), 0);
        outgoing.sent(91);

        outgoing.changed(&domain, prefix(), 121).unwrap();
        let bytes = outgoing
            .prepare(&mut domain, Some(&replaced_policy), target, 121)
            .unwrap()
            .unwrap();
        assert!(matches!(
            receive(&mut session, &bytes).changes().next(),
            Some(bgp::Change::Announce { prefix: restored, .. }) if restored == prefix()
        ));
        outgoing.sent(121);
        // 巨型合法编辑应只撤回此前路由，不能把容量问题升级为整个会话的格式错误。
        let mut excessive = parameters.export.clone().unwrap();
        let RouteAction::Permit {
            as_path_prepend, ..
        } = &mut excessive.rules[0].action
        else {
            unreachable!()
        };
        *as_path_prepend = Some(vec![NonZeroU32::new(80000).unwrap(); 17000].into_boxed_slice());
        outgoing.changed(&domain, prefix(), 151).unwrap();
        let withdrawal = outgoing
            .prepare(&mut domain, Some(&excessive), target, 151)
            .unwrap()
            .unwrap();
        let withdrawal = Update::parse(&withdrawal[19..]).unwrap();
        assert_eq!(withdrawal.withdrawn().unwrap().prefixes().count(), 1);
        assert_eq!(withdrawal.announced().unwrap().prefixes().count(), 0);
    }
    let target = bgp::ExportContext {
        local_as: 65000,
        target: Peer {
            asn: 65000,
            kind: bgp::PeerKind::Internal,
            ..context().target
        },
        ..context()
    };
    let mut outgoing = AdjRibOut::default();
    outgoing.changed(&domain, prefix(), 0).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, parameters.export.as_ref(), target, 0)
            .unwrap()
            .is_some()
    );
    outgoing.sent(0);
    let mut loop_policy = parameters.export.clone().unwrap();
    let RouteAction::Permit {
        as_path_exclude,
        as_path_replace,
        ..
    } = &mut loop_policy.rules[0].action
    else {
        unreachable!()
    };
    *as_path_exclude = None;
    *as_path_replace = Some(Box::new(loop_replace));
    outgoing.changed(&domain, prefix(), 31).unwrap();
    let bytes = outgoing
        .prepare(&mut domain, Some(&loop_policy), target, 31)
        .unwrap()
        .unwrap();
    let withdrawal = Update::parse(&bytes[19..]).unwrap();
    assert_eq!(withdrawal.withdrawn().unwrap().prefixes().count(), 1);
    assert_eq!(withdrawal.announced().unwrap().prefixes().count(), 0);
    assert_eq!(
        domain
            .peer_routes(first.id)
            .unwrap()
            .get(prefix(), None)
            .unwrap()
            .attributes(),
        &original
    );
}

#[test]
fn selected_sources_follow_rib_changes_preserve_metrics_and_never_feed_back_to_bgp() {
    use crate::bgp_peer::receive;
    use crate::routing::{context, receiver};
    use bgp::{AdjRibOut, ExportContext, RoutingDomain, SelectionContext};
    use router_protocol::{
        routing as rib,
        tracking::{
            IpPrefix,
            routes::{Change, Changes, Selector},
        },
    };
    use routing_rib::sources::Routes;
    let configuration = router_protocol::bgp::Configuration::parse(br#"{
      "local_as":65000,"identifier":"192.0.2.1",
      "redistribute":{"connected":true,"protocols":[89,4,4]},
      "neighbors":{"peer":{"address":"192.0.2.3","asn":65002,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":true,"export":{"default":"permit"}}}
    }"#).unwrap();
    let selector = configuration.redistribute.as_ref().unwrap();
    assert_eq!(selector.protocols, [4, 89]);
    assert!(router_protocol::bgp::Configuration::parse(br#"{"local_as":65000,"identifier":"192.0.2.1","neighbors":{},"redistribute":{"protocols":[186]}}"#).is_err());
    let mut domain = RoutingDomain::try_new(
        SelectionContext::new(65000, configuration.identifier, None, None).unwrap(), [false; 2])
    .unwrap();
    domain
        .configure_redistribution(configuration.redistribute.clone())
        .unwrap();
    let mut rib = Routes::default();
    let static_id = rib.reserve_source().unwrap();
    let bgp_id = rib.reserve_source().unwrap();
    let ospf_id = rib.reserve_source().unwrap();
    let fact = network_protocol::InterfaceSnapshot {
        index: 2,
        name: [0; 16],
        loopback: false,
        vlan: false,
        mtu: 1500,
        mac: [2, 0, 0, 0, 0, 2],
        address: Some("10.0.0.2".parse().unwrap()),
        prefix_length: 24,
        broadcast: "10.0.0.255".parse().unwrap(),
        up: true,
        link: None,
        driver: None,
    };
    let source = |id, protocol, network: &str, metric| {
        let members = [rib::Member {
            target: rib::Target::Direct {
                interface_index: 2,
                gateway: Some("10.0.0.1".parse().unwrap()),
                on_link: false,
            },
            weight: 1,
        }];
        let body = rib::Batch::encode(
            rib::Family::Ipv4,
            &[rib::Attributes {
                preference: 1,
                metric,
            }],
            [rib::NextHop::new(rib::Action::Forward, &members)],
            [rib::Route {
                network: network.parse().unwrap(),
                length: 24,
                attributes: 0,
                next_hop: 0,
            }],
        )
        .unwrap();
        Routes::capture_source(id, id, protocol, 1, rib::Batch::parse(&body).unwrap()).unwrap()
    };
    let mut facts = routing_rib::sources::Facts {
        interfaces: vec![fact],
        addresses: network_protocol::AddressSnapshot::capture(core::iter::empty()).unwrap(),
        neighbors: network_protocol::NeighborSnapshot::capture(core::iter::empty()).unwrap(),
    };
    rib = rib
        .prepare_sources(&facts, Some(source(static_id, 4, "198.51.100.0", 17)), &[])
        .unwrap()
        .next;
    rib = rib
        .prepare_sources(&facts, Some(source(bgp_id, 186, "203.0.113.0", 0)), &[])
        .unwrap()
        .next;
    rib = rib
        .prepare_sources(&facts, Some(source(ospf_id, 89, "192.0.2.0", 55)), &[])
        .unwrap()
        .next;
    let original = rib.snapshot().unwrap().clone();
    // 来源身份绑定于观察能力，即使一般读者允许该协议，也不能把自己的来源送回自己。
    let broad = Selector {
        connected: true,
        protocols: vec![4, 89, 186],
    };
    let excluded =
        Changes::capture(original.route_changes(None, &broad, Some(bgp_id)).unwrap()).unwrap();
    assert!(
        excluded
            .iter()
            .all(|change| !matches!(change, Change::Announce(route) if route.source == bgp_id))
    );
    let initial = Changes::capture(
        original
            .route_changes(None, selector, Some(bgp_id))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(initial.iter().len(), 3); // 连接网络、静态和另一协议；不是主机地址/广播记录。
    let changed = domain.apply_redistribution(&initial).unwrap();
    assert_eq!(domain.redistributed_len(), 3);
    assert!(
        domain.selected().is_empty(),
        "redistribution must not create a self-supporting forwarding source"
    );
    let target = ExportContext {
        local_as: 65000,
        interval_ns: 0,
        ..context()
    };
    let mut outgoing = AdjRibOut::default();
    for prefix in &changed {
        outgoing.changed(&domain, *prefix, 0).unwrap();
    }
    assert!(
        outgoing
            .prepare(&mut domain, None, target, 0)
            .unwrap()
            .is_none(),
        "no peer export policy must remain deny"
    );
    outgoing.reconsider(wire::nlri::Family::Ipv4);
    let mut peer = receiver(target);
    let policy = configuration
        .neighbors
        .values()
        .next()
        .unwrap()
        .export
        .as_ref()
        .unwrap();
    let mut seen = std::collections::BTreeSet::new();
    while outgoing.next_deadline().is_some() {
        if let Some(bytes) = outgoing
            .prepare(&mut domain, Some(policy), target, 0)
            .unwrap()
        {
            let received = receive(&mut peer, &bytes);
            assert!(
                received
                    .path()
                    .unwrap()
                    .segments()
                    .flat_map(|segment| segment.asns())
                    .eq([65000])
            );
            let update = packet_wire::bgp::update::Update::parse(&bytes[19..]).unwrap();
            let attributes = update.attributes().unwrap().collect::<Vec<_>>();
            assert!(
                attributes
                    .iter()
                    .any(|attribute| attribute.code == 1 && attribute.data == [2])
            );
            for prefix in update.announced().unwrap().prefixes() {
                let route = domain.redistributed_route(prefix).unwrap();
                assert!(
                    attributes.iter().any(|attribute| attribute.code == 4
                        && attribute.data == route.metric.to_be_bytes())
                );
                assert!(seen.insert(prefix));
            }
            outgoing.sent(1);
        }
    }
    assert_eq!(seen.len(), 3);
    assert_eq!(outgoing.len(), 3);
    let prefix = packet_wire::bgp::nlri::Prefix {
        address: "198.51.100.0".parse().unwrap(),
        length: 24,
    };
    let mut edited = domain.redistributed_route(prefix).unwrap();
    edited.metric = 99;
    let invalid = Changes::capture(
        Changes::encode(
            initial.generation(),
            initial.generation() + 1,
            [
                Change::Announce(edited),
                Change::Withdraw(IpPrefix {
                    network: "203.0.113.0".parse().unwrap(),
                    length: 24,
                }),
            ]
            .into_iter(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(domain.apply_redistribution(&invalid).is_err());
    assert_eq!(domain.redistributed_route(prefix).unwrap().metric, 17);
    assert_eq!(
        domain.redistribution_request().unwrap().1,
        initial.generation()
    );
    rib = rib
        .prepare_sources(&facts, Some(source(static_id, 4, "198.51.100.0", 23)), &[])
        .unwrap()
        .next;
    let updated = rib.snapshot().unwrap().clone();
    let delta = Changes::capture(
        updated
            .route_changes(Some(&original), selector, Some(bgp_id))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(delta.iter().len(), 1);
    let changed = domain.apply_redistribution(&delta).unwrap();
    assert_eq!(domain.redistributed_route(prefix).unwrap().metric, 23);
    for prefix in changed {
        outgoing.changed(&domain, prefix, 2).unwrap();
    }
    let bytes = outgoing
        .prepare(&mut domain, Some(policy), target, 2)
        .unwrap()
        .unwrap();
    receive(&mut peer, &bytes);
    assert!(
        packet_wire::bgp::update::Update::parse(&bytes[19..])
            .unwrap()
            .attributes()
            .unwrap()
            .any(|attribute| attribute.code == 4 && attribute.data == 23u32.to_be_bytes())
    );
    outgoing.sent(3);
    let unchanged = Changes::capture(
        updated
            .route_changes(Some(&updated), selector, Some(bgp_id))
            .unwrap(),
    )
    .unwrap();
    assert!(domain.apply_redistribution(&unchanged).unwrap().is_empty());
    rib = rib
        .prepare_sources(&facts, None, &[static_id, ospf_id])
        .unwrap()
        .next;
    let removed = rib.snapshot().unwrap().clone();
    let delta = Changes::capture(
        removed
            .route_changes(Some(&updated), selector, Some(bgp_id))
            .unwrap(),
    )
    .unwrap();
    assert!(
        delta
            .iter()
            .all(|change| matches!(change, Change::Withdraw(_)))
    );
    for prefix in domain.apply_redistribution(&delta).unwrap() {
        outgoing.changed(&domain, prefix, 4).unwrap();
    }
    let bytes = outgoing
        .prepare(&mut domain, Some(policy), target, 4)
        .unwrap()
        .unwrap();
    assert_eq!(
        packet_wire::bgp::update::Update::parse(&bytes[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .count(),
        2
    );
    outgoing.sent(5);
    assert_eq!(outgoing.len(), 1);
    facts.interfaces[0].up = false;
    rib = rib.prepare_sources(&facts, None, &[]).unwrap().next;
    let delta = Changes::capture(
        rib.snapshot()
            .unwrap()
            .route_changes(Some(&removed), selector, Some(bgp_id))
            .unwrap(),
    )
    .unwrap();
    for prefix in domain.apply_redistribution(&delta).unwrap() {
        outgoing.changed(&domain, prefix, 6).unwrap();
    }
    let bytes = outgoing
        .prepare(&mut domain, Some(policy), target, 6)
        .unwrap()
        .unwrap();
    assert_eq!(
        packet_wire::bgp::update::Update::parse(&bytes[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .count(),
        1
    );
    outgoing.sent(7);
    assert!(outgoing.is_empty());
}
