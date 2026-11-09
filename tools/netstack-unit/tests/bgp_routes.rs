//! 用实际 Session 的接收决定保护候选替换/撤回与共享引用生命周期；格式/驻留用例不拥有接收表。
use ::bgp::{
    AdjRibIn, AttributePool, Candidate, Effect, Handling, Input, LocRib, LocRibError, NextHop,
    OutOfMemory, Peer, PeerKind, Reachability, SelectedRoute, SelectionContext, Selector,
};
use core::net::IpAddr;
use packet_wire::bgp::{
    attributes::{Attribute, MpReach, MpUnreach},
    nlri::{Family, Nlri, Prefix},
};
mod bgp_peer;
use bgp_peer::{attributes, base, established, receive, update};

fn prefix(address: &str, length: u8) -> Prefix {
    Prefix {
        address: address.parse().unwrap(),
        length,
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
    assert!(rib.iter().map(|(prefix, _)| prefix).eq(expected));
    assert_eq!(
        rib.get(expected[0]).unwrap().next_hop().address,
        "192.0.2.2".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        rib.get(expected[2]).unwrap().next_hop().address,
        "2001:db8::1".parse::<IpAddr>().unwrap()
    );
    assert!(
        rib.iter()
            .all(|(_, route)| route.attributes().path().contains(65001))
    );
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
    assert!(rib.get(expected[1]).is_none());
    assert!(rib.get(expected[3]).is_some());
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
    let changed = rib.get(prefix("203.0.113.0", 24)).unwrap();
    assert_eq!(changed.attributes().med(), Some(7));
    assert_eq!(
        changed.next_hop().address,
        "192.0.2.9".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        rib.get(prefix("198.51.100.0", 24))
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
    let context = SelectionContext::new(local.local_as, local.identifier, None).unwrap();
    let a_open = a.negotiated().unwrap();
    let b_open = b.negotiated().unwrap();
    let a_peer = Peer {
        id: 1,
        address: "192.0.2.2".parse().unwrap(),
        identifier: a_open.peer_identifier,
        asn: a_open.peer_as,
        kind: PeerKind::External,
    };
    let b_peer = Peer {
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
                        peer: &a_peer,
                        next_hop: first.get(prefix("0.0.0.0", 0)).unwrap().next_hop(),
                        attributes: first.get(prefix("0.0.0.0", 0)).unwrap().attributes(),
                        preference: Some(100),
                        reachability
                    },
                    Candidate {
                        peer: &b_peer,
                        next_hop: second.get(prefix("0.0.0.0", 0)).unwrap().next_hop(),
                        attributes: second.get(prefix("0.0.0.0", 0)).unwrap().attributes(),
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
                    peer: &b_peer,
                    next_hop: second.get(prefix("0.0.0.0", 0)).unwrap().next_hop(),
                    attributes: second.get(prefix("0.0.0.0", 0)).unwrap().attributes(),
                    preference: Some(100),
                    reachability,
                }]
            )
            .unwrap(),
        Some(0)
    );
    assert_eq!(retired.len(), 2);
    assert!(retired.get(prefix("0.0.0.0", 0)).is_some());
    assert!(retired.get(prefix("0.0.0.0", 32)).is_some());
    drop(retired);
    assert_eq!(second.len(), 2);
    assert_eq!(pool.len(), 1);
    let retired = second.retire();
    assert_eq!(selector.select(context, &[]).unwrap(), None);
    let retained = retired.iter().next().unwrap().1.attributes().clone();
    drop(retired);
    assert!(second.is_empty());
    assert_eq!(pool.len(), 1);
    assert!(retained.path().contains(65001));
    drop(retained);
    assert!(pool.is_empty());
    assert_eq!(pool.path_count(), 0);
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
    let context = SelectionContext::new(config.local_as, config.identifier, None).unwrap();
    let peer = Peer {
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
    let received = adj.get(first_prefix).unwrap();
    let candidate = Candidate {
        peer: &peer,
        attributes: received.attributes(),
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
            peer: &backup,
            ..candidate
        }],
    )
    .unwrap()
    .unwrap();
    // 属性不含 NLRI/下一跳；IPv6 后政策投影同样复用其路径/团体，不用错误的 IPv4 目标占位。
    let ipv6_hop = NextHop {
        address: "2001:db8::1".parse().unwrap(),
        link_local: None,
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
    assert_eq!(prepared.commit(), initial);
    let prepared = loc
        .prepare(Family::Ipv6, [Ok((ipv6_prefix, Some(ipv6.clone())))])
        .unwrap();
    assert!(prepared.iter().map(|(prefix, _)| prefix).eq([ipv6_prefix]));
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
