//! 标量筛选/编辑沿真实接收、确认选择和出口报文；与集合/路径共用同一会话装配。
use super::{
    bgp_peer::{attributes, base, established, receive, update},
    routing::{choose, context, domain, prefix, receiver},
};
use bgp::{AdjRibOut, Peer, Reachability, SourcePeer};
use core::num::NonZeroU32;
use packet_wire::bgp::{self as wire, update::Update};
use router_protocol::bgp::RouteAction;

#[test]
fn scalar_policies_change_real_selection_and_outgoing_attributes_without_mutating_received_routes()
{
    use router_protocol::bgp::{MedAction, Origin};
    let (mut domain, mut first_session) = domain();
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
    let bytes = update(&[], &attributes(&base(false, true)), &[24, 198, 51, 100]);
    domain
        .receive(1, &receive(&mut first_session, &bytes))
        .unwrap();
    let mut second_session = established(false, true);
    domain
        .receive(second.id, &receive(&mut second_session, &bytes))
        .unwrap();
    let configured = router_protocol::bgp::Configuration::parse(br#"{
        "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
            "address":"192.0.2.5","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":true,
            "import":{"default":"deny","preference":100,"rules":[{"origin":"igp","local_preference":{"start":100,"end":100},"as_path_length":{"start":1,"end":1},"action":{"kind":"permit","origin":"igp","med":{"kind":"set","value":7}}}]},
            "export":{"default":"deny","rules":[{"origin":"igp","med":{"start":1,"end":1},"local_preference":{"start":100,"end":100},"as_path_length":{"start":1,"end":1},"action":{"kind":"permit","origin":"incomplete","med":{"kind":"set","value":0}}}]}
        }}}"#).unwrap();
    let parameters = configured.neighbors.values().next().unwrap();
    assert_eq!(
        router_protocol::bgp::Configuration::parse(&configured.encode().unwrap()).unwrap(),
        configured
    );
    let raw = domain
        .peer_routes(second.id)
        .unwrap()
        .get(prefix(), None)
        .unwrap()
        .attributes()
        .clone();
    // 缺失 MED 不等于显式零；其他标量不匹配时也不能套用许可动作。
    let received = domain
        .peer_routes(second.id)
        .unwrap()
        .get(prefix(), None)
        .unwrap();
    for field in ["med", "origin", "local_preference", "as_path_length"] {
        let mut policy = parameters.import.clone().unwrap();
        let rule = &mut policy.rules[0];
        match field {
            "med" => rule.med = Some(0..=0),
            "origin" => rule.origin = Some(Origin::Incomplete),
            "local_preference" => rule.local_preference = Some(0..=99),
            "as_path_length" => rule.as_path_length = Some(0..=0),
            _ => unreachable!(),
        }
        assert!(
            bgp::import_route(Some(&policy), prefix(), &second, received).is_none(),
            "{field}"
        );
    }
    for field in ["med", "local_preference", "as_path_length"] {
        let mut invalid = configured.clone();
        let neighbor = invalid.neighbors.values_mut().next().unwrap();
        let rule = &mut neighbor.import.as_mut().unwrap().rules[0];
        let range = match field {
            "med" => &mut rule.med,
            "local_preference" => &mut rule.local_preference,
            "as_path_length" => &mut rule.as_path_length,
            _ => unreachable!(),
        };
        *range = Some(core::ops::RangeInclusive::new(2, 1));
        assert!(invalid.validate().unwrap_err().path.ends_with(field));
        assert!(invalid.encode().is_err());
    }
    let decision = bgp::import_route(
        parameters.import.as_ref(),
        prefix(),
        &second,
        domain
            .peer_routes(second.id)
            .unwrap()
            .get(prefix(), None)
            .unwrap(),
    )
    .unwrap();
    let preferred = domain.project_attributes(&raw, decision.changes()).unwrap();
    let worse = domain
        .project_attributes(
            &raw,
            bgp::AttributeChanges::scalars(
                Some(Origin::Incomplete),
                Some(MedAction::Set { value: 0 }),
            ),
        )
        .unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, peer, _, route| {
            Ok((
                route.next_hop(),
                Some(100),
                Reachability::Resolved {
                    interior_cost: None,
                },
                if peer.id == second.id {
                    &preferred
                } else {
                    &worse
                },
            ))
        })
        .unwrap()
        .commit();
    assert_eq!(domain.selected().get(prefix()).unwrap().peer(), second.id); // ORIGIN 优先于 MED。
    assert_eq!(raw.origin(), 0);
    assert_eq!(raw.med(), None);
    assert_eq!(
        domain
            .peer_routes(second.id)
            .unwrap()
            .get(prefix(), None)
            .unwrap()
            .attributes(),
        &raw
    );
    let better_med = domain
        .project_attributes(
            &raw,
            bgp::AttributeChanges::scalars(Some(Origin::Igp), Some(MedAction::Set { value: 1 })),
        )
        .unwrap();
    domain.reconsider([prefix()]).unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, peer, _, route| {
            Ok((
                route.next_hop(),
                Some(100),
                Reachability::Resolved {
                    interior_cost: None,
                },
                if peer.id == second.id {
                    &preferred
                } else {
                    &better_med
                },
            ))
        })
        .unwrap()
        .commit();
    assert_eq!(domain.selected().get(prefix()).unwrap().peer(), first.id); // 相同相邻 AS/ORIGIN 后比较改写的 MED。
    let mut outgoing = AdjRibOut::default();
    outgoing.changed(&domain, prefix(), 0).unwrap();
    let message = outgoing
        .prepare(&mut domain, parameters.export.as_ref(), context(), 0)
        .unwrap()
        .unwrap();
    let mut remote = receiver(context());
    let received = receive(&mut remote, &message);
    assert!(received.path().is_some());
    let actual = Update::parse(&message[19..])
        .unwrap()
        .attributes()
        .unwrap()
        .collect::<Vec<_>>();
    assert!(
        actual
            .iter()
            .any(|attribute| attribute.code == 1 && attribute.data == [2])
    );
    assert!(
        actual
            .iter()
            .any(|attribute| attribute.code == 4 && attribute.data == [0, 0, 0, 0])
    );
    outgoing.sent(1);
    let mut removed = parameters.export.clone().unwrap();
    removed.rules[0].action = RouteAction::Permit {
        preference: None,
        next_hop: None,
        origin: Some(Origin::Incomplete),
        med: Some(MedAction::Remove),
        as_path_prepend: None,
        as_path_exclude: None,
        as_path_replace: None,
        communities: None,
        extended_communities: None,
        large_communities: None,
    };
    outgoing.changed(&domain, prefix(), 31).unwrap();
    let message = outgoing
        .prepare(&mut domain, Some(&removed), context(), 31)
        .unwrap()
        .unwrap();
    assert!(
        !Update::parse(&message[19..])
            .unwrap()
            .attributes()
            .unwrap()
            .any(|attribute| attribute.code == 4)
    );
    assert_eq!(
        domain.selected().get(prefix()).unwrap().attributes().med(),
        Some(1)
    );
    assert_eq!(
        domain
            .peer_routes(first.id)
            .unwrap()
            .get(prefix(), None)
            .unwrap()
            .attributes()
            .med(),
        None
    );
    outgoing.sent(31);
    // 无政策改写重新选路，不能把旧的派生值写回接收表后遗留。
    domain.reconsider([prefix()]).unwrap();
    choose(&mut domain);
    assert_eq!(
        domain
            .selected()
            .get(prefix())
            .unwrap()
            .attributes()
            .origin(),
        0
    );
    assert_eq!(
        domain.selected().get(prefix()).unwrap().attributes().med(),
        None
    );
    // 选路恢复后 MED 已缺失，出口条件不再满足；必须撤回此前已经完整发送的宣告。
    outgoing.changed(&domain, prefix(), 62).unwrap();
    let withdrawn = outgoing
        .prepare(&mut domain, parameters.export.as_ref(), context(), 62)
        .unwrap()
        .unwrap();
    assert_eq!(
        receive(&mut remote, &withdrawn).changes().next(),
        Some(bgp::Change::Withdraw(prefix(), None))
    );
    outgoing.sent(62);
    // eBGP 接收没有 LOCAL_PREF，出口仍匹配 Loc-RIB 确认的本地 degree，而不是原始字段或旧默认值。
    domain.reconsider([prefix()]).unwrap();
    domain
        .prepare(wire::nlri::Family::Ipv4, |_, _, _, route| {
            Ok((
                route.next_hop(),
                Some(200),
                Reachability::Resolved {
                    interior_cost: None,
                },
                route.attributes(),
            ))
        })
        .unwrap()
        .commit();
    let mut policy = parameters.export.clone().unwrap();
    policy.rules[0].med = None;
    policy.rules[0].local_preference = Some(200..=200);
    outgoing.changed(&domain, prefix(), 93).unwrap();
    let announced = outgoing
        .prepare(&mut domain, Some(&policy), context(), 93)
        .unwrap()
        .unwrap();
    assert!(
        matches!(receive(&mut remote,&announced).changes().next(),Some(bgp::Change::Announce{prefix:actual,..}) if actual==prefix())
    );
}
