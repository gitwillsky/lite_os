//! 沿生产 Session/接收表/选路和出口 codec 检查真正发送的报文；不增加独立网络或生产测试入口。
mod bgp_peer;
#[path = "bgp_peer/routing.rs"]
mod routing;
use bgp::{AdjRibOut, ExportContext, Peer, PeerKind, RoutingDomain, SelectionContext, SourcePeer};
use bgp_peer::{attributes, base, receive, update};
use core::num::NonZeroU32;
use packet_wire::bgp::{
    attributes::{AsnWidth, Attribute, Value},
    update::Update,
};
use router_protocol::bgp::RouteAction;
use routing::{choose, context, domain, learn, policy, prefix, receiver};

#[test]
fn configured_export_reaches_a_real_peer_with_as4_boundary_and_attributes_preserved() {
    let (mut domain, mut session) = domain();
    learn(
        &mut domain,
        &mut session,
        &[
            Attribute {
                flags: 0x80,
                code: 4,
                data: &[0, 0, 0, 17],
            },
            Attribute {
                flags: 0xc0,
                code: 8,
                data: &[0, 0, 0, 42],
            },
            Attribute {
                flags: 0xc0,
                code: 16,
                data: &[0, 2, 0, 1, 0, 0, 0, 9, 0x40, 2, 0, 1, 0, 0, 0, 9],
            },
            Attribute {
                flags: 0xc0,
                code: 99,
                data: &[1, 2, 3],
            },
        ],
    );
    let configuration = router_protocol::bgp::Configuration::parse(br#"{"local_as":4200000001,"identifier":"192.0.2.9",
        "neighbors":{"downstream":{"address":"192.0.2.3","asn":65002,"preference":20,"hold_seconds":90,
        "retry_seconds":5,"passive":true,"export":{"default":"permit"},"advertisement_seconds":0}}}"#).unwrap();
    let configured = configuration
        .neighbors
        .into_values()
        .next()
        .unwrap()
        .export
        .unwrap();
    for width in [AsnWidth::Two, AsnWidth::Four] {
        let context = ExportContext { width, ..context() };
        let mut outgoing = AdjRibOut::new();
        outgoing.changed(prefix(), 10).unwrap();
        let message = outgoing
            .prepare(&mut domain, Some(&configured), context, 10)
            .unwrap()
            .unwrap();
        assert!(outgoing.is_empty()); // 编码成功不是 TCP 已完整发送。
        let mut remote = receiver(context);
        let received = receive(&mut remote, &message);
        assert_eq!(
            received
                .path()
                .unwrap()
                .segments()
                .flat_map(|segment| segment.asns())
                .collect::<Vec<_>>(),
            [4200000001, 65001]
        );
        let decoded = Update::parse(&message[19..]).unwrap();
        assert_eq!(
            decoded.announced().unwrap().prefixes().collect::<Vec<_>>(),
            [prefix()]
        );
        let list = decoded.attributes().unwrap();
        let decoded = list
            .map(|attribute| (attribute.code, attribute))
            .collect::<Vec<_>>();
        assert!(
            !decoded
                .iter()
                .any(|(code, _)| matches!(code, 4 | 5 | 9 | 10))
        );
        assert!(
            decoded
                .iter()
                .any(|(code, attribute)| *code == 3 && attribute.data == [192, 0, 2, 9])
        );
        assert!(
            decoded
                .iter()
                .any(|(code, attribute)| *code == 16 && attribute.data.len() == 8)
        );
        assert!(
            decoded
                .iter()
                .any(|(code, attribute)| *code == 99 && attribute.flags & 0xe0 == 0xe0)
        );
        outgoing.sent(11);
        assert_eq!(outgoing.len(), 1);
        outgoing.changed(prefix(), 12).unwrap();
        assert!(
            outgoing
                .prepare(&mut domain, Some(&configured), context, 12)
                .unwrap()
                .is_none()
        );
        assert_eq!(outgoing.next_deadline(), None);
    }
    // 原始消息仍合法，但加本地 AS 后超过对端上限：只撤回此目标，不终止域或发超长帧。
    let mut outgoing = AdjRibOut::new();
    outgoing.changed(prefix(), 0).unwrap();
    outgoing
        .prepare(&mut domain, Some(&configured), context(), 0)
        .unwrap()
        .unwrap();
    outgoing.sent(1);
    let large = vec![0; 4044];
    learn(
        &mut domain,
        &mut session,
        &[Attribute {
            flags: 0xc0,
            code: 99,
            data: &large,
        }],
    );
    outgoing.changed(prefix(), 31).unwrap();
    let withdrawn = outgoing
        .prepare(&mut domain, Some(&configured), context(), 31)
        .unwrap()
        .unwrap();
    assert_eq!(
        Update::parse(&withdrawn[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .count(),
        1
    );
    assert_eq!(outgoing.rejected_route(), Some((prefix(), packet_wire::PacketError::Capacity)));
    outgoing.sent(32);
    assert!(outgoing.is_empty());
    assert!(
        outgoing
            .prepare(&mut domain, Some(&configured), context(), 62)
            .unwrap()
            .is_none()
    );
    assert_eq!(outgoing.next_deadline(), None);
}
#[test]
fn queued_changes_preserve_partial_frames_obey_per_destination_interval_and_withdraw_then_reannounce()
 {
    let (mut domain, mut session) = domain();
    learn(&mut domain, &mut session, &[]);
    let mut outgoing = AdjRibOut::new();
    outgoing.changed(prefix(), 10).unwrap();
    outgoing.finish_initial(packet_wire::bgp::nlri::Family::Ipv4);
    let first = outgoing
        .prepare(&mut domain, Some(&policy()), context(), 10)
        .unwrap()
        .unwrap();
    assert!(!first.is_empty());
    let withdrawn = update(&[24, 198, 51, 100], &[], &[]);
    domain
        .receive(1, &receive(&mut session, &withdrawn))
        .unwrap();
    choose(&mut domain);
    outgoing.changed(prefix(), 11).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 11)
            .unwrap()
            .is_none()
    );
    outgoing.sent(12);
    assert_eq!(outgoing.len(), 1);
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 13)
            .unwrap()
            .is_none()
    );
    assert_eq!(outgoing.next_deadline(), Some(42));
    let withdrawn = outgoing
        .prepare(&mut domain, Some(&policy()), context(), 42)
        .unwrap()
        .unwrap();
    let decoded = Update::parse(&withdrawn[19..]).unwrap();
    assert_eq!(
        decoded.withdrawn().unwrap().prefixes().collect::<Vec<_>>(),
        [prefix()]
    );
    outgoing.sent(43);
    assert!(outgoing.is_empty());
    learn(&mut domain, &mut session, &[]);
    outgoing.changed(prefix(), 44).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 44)
            .unwrap()
            .is_none()
    );
    assert_eq!(outgoing.next_deadline(), Some(73));
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 73)
            .unwrap()
            .is_some()
    );
    outgoing.sent(74);
    let end = outgoing
        .prepare(&mut domain, Some(&policy()), context(), 74)
        .unwrap()
        .unwrap();
    assert_eq!(end.len(), 23);
    outgoing.sent(75);
    assert_eq!(outgoing.next_deadline(), None);
    // 普通不变更新可去重；对端显式刷新必须重发，仍按真实 sent_at 限速。
    outgoing.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 76)
            .unwrap()
            .is_none()
    );
    assert_eq!(outgoing.next_deadline(), Some(104));
    let replay = outgoing
        .prepare(&mut domain, Some(&policy()), context(), 104)
        .unwrap()
        .unwrap();
    assert_eq!(
        Update::parse(&replay[19..])
            .unwrap()
            .announced()
            .unwrap()
            .prefixes()
            .collect::<Vec<_>>(),
        [prefix()]
    );
    outgoing.refresh(packet_wire::bgp::nlri::Family::Ipv4, false); // 这份帧尚未完整送出；新请求不能在 sent 时一起抹掉。
    assert_eq!(outgoing.next_deadline(), None);
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 105)
            .unwrap()
            .is_none()
    );
    outgoing.sent(106);
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 107)
            .unwrap()
            .is_none()
    );
    assert_eq!(outgoing.next_deadline(), Some(136));
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 136)
            .unwrap()
            .is_some()
    );
    outgoing.sent(137);
    // 在重发前撤回或政策拒绝不能从旧发送记录“复活”路由。
    outgoing.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    let denied = router_protocol::bgp::ExportPolicy {
        default: router_protocol::policy::Disposition::Deny,
        rules: vec![],
    };
    assert!(
        outgoing
            .prepare(&mut domain, Some(&denied), context(), 138)
            .unwrap()
            .is_none()
    );
    let removed = outgoing
        .prepare(&mut domain, Some(&denied), context(), 167)
        .unwrap()
        .unwrap();
    assert_eq!(
        Update::parse(&removed[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .collect::<Vec<_>>(),
        [prefix()]
    );
    outgoing.sent(168);
    assert_eq!(outgoing.len(), 0);
    // 同帧内一个目标在部分发送期间撤回；其余目标不能丢失、重复确认或继承该目标的间隔。
    let (mut batch_domain, mut batch_session) = routing::domain();
    let nlri = [32, 203, 0, 113, 1, 32, 203, 0, 113, 2];
    let bytes = update(&[], &attributes(&base(false, true)), &nlri);
    batch_domain.receive(1, &receive(&mut batch_session, &bytes)).unwrap();
    choose(&mut batch_domain);
    let targets = batch_domain.selected().iter().map(|(prefix, _)| prefix).collect::<Vec<_>>();
    let mut output = AdjRibOut::new();
    for target in &targets { output.changed(*target, 0).unwrap(); }
    let first = output.prepare(&mut batch_domain, Some(&policy()), context(), 0).unwrap().unwrap();
    assert_eq!(Update::parse(&first[19..]).unwrap().announced().unwrap().prefixes().count(), 2);
    assert!(output.is_empty());
    let withdrawn = update(&nlri[..5], &[], &[]);
    batch_domain.receive(1, &receive(&mut batch_session, &withdrawn)).unwrap();
    choose(&mut batch_domain);
    output.changed(targets[0], 1).unwrap();
    assert!(output.prepare(&mut batch_domain, Some(&policy()), context(), 1).unwrap().is_none());
    output.sent(2);
    assert_eq!(output.len(), 2);
    assert!(output.prepare(&mut batch_domain, Some(&policy()), context(), 3).unwrap().is_none());
    assert_eq!(output.next_deadline(), Some(32));
    let withdrawn = output.prepare(&mut batch_domain, Some(&policy()), context(), 32).unwrap().unwrap();
    assert!(Update::parse(&withdrawn[19..]).unwrap().withdrawn().unwrap().prefixes().eq([targets[0]]));
    output.sent(33);
    assert_eq!(output.len(), 1);
    // 剩余目标始终真实已宣告；变更登记仍能正确去重。
    output.changed(targets[1], 34).unwrap();
    assert!(output.prepare(&mut batch_domain, Some(&policy()), context(), 34).unwrap().is_none());
    assert_eq!(output.len(), 1);

}
#[test]
fn explicit_policy_cannot_bypass_well_known_communities_or_internal_split_horizon() {
    let (mut domain, mut session) = domain();
    learn(&mut domain, &mut session, &[]);
    let mut outgoing = AdjRibOut::new();
    outgoing.changed(prefix(), 0).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, None, context(), 0)
            .unwrap()
            .is_none()
    );
    for community in [0xffff_ff01u32, 0xffff_ff02, 0xffff_ff03] {
        learn(
            &mut domain,
            &mut session,
            &[Attribute {
                flags: 0xc0,
                code: 8,
                data: &community.to_be_bytes(),
            }],
        );
        outgoing.changed(prefix(), 1).unwrap();
        assert!(
            outgoing
                .prepare(&mut domain, Some(&policy()), context(), 1)
                .unwrap()
                .is_none()
        );
    }
    learn(&mut domain, &mut session, &[]);
    let mut internal = context();
    internal.target.asn = 65000;
    internal.target.kind = PeerKind::Internal;
    outgoing.changed(prefix(), 2).unwrap();
    let message = outgoing
        .prepare(&mut domain, Some(&policy()), internal, 2)
        .unwrap()
        .unwrap();
    let mut list = Update::parse(&message[19..]).unwrap().attributes().unwrap();
    assert!(list.any(|attribute| matches!(
        attribute.decode(AsnWidth::Four),
        Ok(Value::LocalPreference(100))
    )));
    outgoing.sent(3);
    let mut denied = policy();
    denied.rules.push(router_protocol::bgp::RouteRule {
        origin: None, med: None, local_preference: None, as_path_length: None,
        prefix: None,
        as_path_contains: vec![],
        as_path_regex: None,
        communities: vec![],
        extended_communities: vec![],
        large_communities: vec![],
        action: RouteAction::Deny,
    });
    outgoing.changed(prefix(), 34).unwrap();
    let message = outgoing
        .prepare(&mut domain, Some(&denied), internal, 34)
        .unwrap()
        .unwrap();
    assert_eq!(
        Update::parse(&message[19..])
            .unwrap()
            .withdrawn()
            .unwrap()
            .prefixes()
            .count(),
        1
    );
}

#[test]
fn internal_routes_are_not_reflected_without_an_explicit_reflector_role() {
    let (mut domain, _) = domain();
    domain.remove_peer(1).unwrap();
    let peer = Peer {
        route_reflector_client: false,
        id: 3,
        asn: 65000,
        kind: PeerKind::Internal,
        address: "192.0.2.4".parse().unwrap(),
        identifier: "192.0.2.4".parse().unwrap(),
    };
    domain
        .add_peer(
            peer,
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    let internal = ExportContext {
        local_as: 65000,
        target: Peer {
            asn: 65000,
            kind: PeerKind::Internal,
            ..context().target
        },
        ..context()
    };
    let mut session = receiver(internal);
    let message = update(&[], &attributes(&base(true, true)), &[24, 198, 51, 100]);
    domain.receive(3, &receive(&mut session, &message)).unwrap();
    choose(&mut domain);
    assert_eq!(domain.selected().len(), 1);
    let mut outgoing = AdjRibOut::new();
    outgoing.changed(prefix(), 0).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), internal, 0)
            .unwrap()
            .is_none()
    );
    outgoing.changed(prefix(), 1).unwrap();
    assert!(
        outgoing
            .prepare(&mut domain, Some(&policy()), context(), 1)
            .unwrap()
            .is_some()
    );
}

#[test]
fn reflector_configuration_controls_propagation_and_wire_attributes_reject_returned_cluster_loops()
{
    let configured = router_protocol::bgp::Configuration::parse(
        br#"{
        "local_as":65000,"identifier":"192.0.2.9","neighbors":{
            "client":{"address":"192.0.2.4","asn":65000,"preference":20,"hold_seconds":90,
                "retry_seconds":5,"passive":true,"route_reflector_client":true,
                "import":{"default":"permit","preference":100},"export":{"default":"permit"}}
        }}"#,
    )
    .unwrap();
    let cluster = configured.reflection_cluster().unwrap();
    assert_eq!(cluster, configured.identifier);
    let mut context = ExportContext {
        local_as: configured.local_as.get(),
        cluster_id: Some(cluster),
        interval_ns: 0,
        target: Peer {
            asn: configured.local_as.get(),
            kind: PeerKind::Internal,
            ..context().target
        },
        ..context()
    };
    for source_client in [false, true] {
        for target_client in [false, true] {
            for previous_origin in [
                None,
                Some("192.0.2.77".parse::<core::net::Ipv4Addr>().unwrap()),
            ] {
                let mut domain = RoutingDomain::try_new(
                    SelectionContext::new(
                        configured.local_as.get(),
                        configured.identifier,
                        configured.reflection_cluster(),
                    None,)
                    .unwrap(),
                )
                .unwrap();
                let source = Peer {
                    id: 3,
                    route_reflector_client: source_client,
                    asn: configured.local_as.get(),
                    kind: PeerKind::Internal,
                    address: "192.0.2.4".parse().unwrap(),
                    identifier: "192.0.2.4".parse().unwrap(),
                };
                domain
                    .add_peer(
                        source,
                        SourcePeer {
                            preference: NonZeroU32::new(20).unwrap(),
                            next_hop_interface: None,
                        },
                    )
                    .unwrap();
                let mut session = receiver(context);
                let old_clusters = [10, 0, 0, 7, 10, 0, 0, 8];
                let origin_bytes = previous_origin.map(|id| id.octets());
                let mut list = base(true, true);
                list.push(Attribute {
                    flags: 0x80,
                    code: 4,
                    data: &[0, 0, 0, 17],
                });
                if let Some(bytes) = &origin_bytes {
                    list.push(Attribute {
                        flags: 0x80,
                        code: 9,
                        data: bytes,
                    });
                    list.push(Attribute {
                        flags: 0x80,
                        code: 10,
                        data: &old_clusters,
                    });
                }
                let incoming = update(&[], &attributes(&list), &[24, 198, 51, 100]);
                domain
                    .receive(source.id, &receive(&mut session, &incoming))
                    .unwrap();
                choose(&mut domain);
                assert_eq!(domain.selected().len(), 1);
                context.target.route_reflector_client = target_client;
                let mut outgoing = AdjRibOut::new();
                outgoing.changed(prefix(), 0).unwrap();
                let message = outgoing
                    .prepare(
                        &mut domain,
                        configured
                            .neighbors
                            .values()
                            .next()
                            .unwrap()
                            .export
                            .as_ref(),
                        context,
                        0,
                    )
                    .unwrap();
                if !source_client && !target_client {
                    assert!(message.is_none());
                    continue;
                }
                let message = message.unwrap();
                outgoing.sent(1);
                let actual = Update::parse(&message[19..])
                    .unwrap()
                    .attributes()
                    .unwrap()
                    .collect::<Vec<_>>();
                let attribute = |code| {
                    actual
                        .iter()
                        .find(|attribute| attribute.code == code)
                        .unwrap()
                };
                let expected_origin = previous_origin.unwrap_or(source.identifier);
                assert_eq!(attribute(9).data, expected_origin.octets());
                let mut expected_clusters = cluster.octets().to_vec();
                if previous_origin.is_some() {
                    expected_clusters.extend_from_slice(&old_clusters);
                }
                assert_eq!(attribute(10).data, expected_clusters);
                assert_eq!(attribute(3).data, [192, 0, 2, 2]);
                assert_eq!(attribute(4).data, [0, 0, 0, 17]);
                assert_eq!(attribute(5).data, [0, 0, 0, 100]);
                assert_eq!(attribute(2).data, base(true, true)[1].data);
                // 不能把反射回给原始起源者；不同连接身份不构成绕过去环的理由。
                let returned_to_origin = ExportContext {
                    target: Peer {
                        identifier: expected_origin,
                        ..context.target
                    },
                    ..context
                };
                let mut origin_output = AdjRibOut::new();
                origin_output.changed(prefix(), 2).unwrap();
                assert!(
                    origin_output
                        .prepare(&mut domain, Some(&policy()), returned_to_origin, 2)
                        .unwrap()
                        .is_none()
                );
                // 同一真实接收/选路路径看到本簇后拒绝该路由，随后必须撤回已发送的旧选择。
                domain
                    .receive(source.id, &receive(&mut session, &message))
                    .unwrap();
                choose(&mut domain);
                assert!(domain.selected().is_empty());
                outgoing.changed(prefix(), 2).unwrap();
                let withdrawn = outgoing
                    .prepare(&mut domain, Some(&policy()), context, 2)
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
            }
        }
    }
}

#[test]
fn route_refresh_replays_a_complete_table_in_bounded_steps_and_coalesces_followup_requests() {
    use bgp::{Effect, Input};
    use packet_wire::bgp::{Frame, Kind};
    let (mut domain, mut session) = domain();
    // 超过两批的目标，保护游标推进、已有工作合并及重复请求不饿死后续前缀。
    let mut announced = Vec::new();
    for index in 0..300u16 {
        announced.extend_from_slice(&[32, 203, 0, (index >> 8) as u8, index as u8]);
    }
    let bytes = update(&[], &attributes(&base(false, true)), &announced);
    domain.receive(1, &receive(&mut session, &bytes)).unwrap();
    // 两种属性交错分布；合批不能依赖网段排序后恰好连续，也不能把另一组的属性套到本组。
    let mut changed_fields = base(false, true);
    changed_fields.iter_mut().find(|attribute| attribute.code == 1).unwrap().data = &[2];
    let changed_fields = attributes(&changed_fields);
    let mut alternate = std::collections::BTreeSet::new();
    for target in announced.as_chunks::<5>().0.iter().step_by(2) {
        let bytes = update(&[], &changed_fields, target);
        domain.receive(1, &receive(&mut session, &bytes)).unwrap();
        alternate.extend(packet_wire::bgp::nlri::Nlri::parse(target, packet_wire::bgp::nlri::Family::Ipv4).unwrap().prefixes());
    }
    choose(&mut domain);
    let context = ExportContext {
        interval_ns: 0,
        ..context()
    };
    let mut output = AdjRibOut::new();
    for (prefix, _) in domain.selected().iter() {
        output.changed(prefix, 0).unwrap();
    }
    let mut initial_frames = 0;
    let mut initial_seen = std::collections::BTreeSet::new();
    let mut remote = receiver(context);
    while output.next_deadline().is_some() {
        if let Some(bytes) = output.prepare(&mut domain, Some(&policy()), context, 0).unwrap() {
            let received = receive(&mut remote, &bytes);
            assert!(received.path().is_some());
            let decoded = Update::parse(&bytes[19..]).unwrap();
            let origin = decoded.attributes().unwrap().find(|attribute| attribute.code == 1).unwrap().data;
            for target in decoded.announced().unwrap().prefixes() {
                assert_eq!(origin, if alternate.contains(&target) { &[2] } else { &[0] });
                assert!(initial_seen.insert(target));
            }
            let before = output.len();
            let count = decoded.announced().unwrap().prefixes().count();
            assert!(bytes.len() <= packet_wire::bgp::STANDARD_LIMIT);
            assert!(output.prepare(&mut domain, Some(&policy()), context, 0).unwrap().is_none());
            output.sent(0);
            assert_eq!(output.len(), before + count);
            initial_frames += 1;
        }
    }
    assert_eq!(output.len(), 300);
    assert_eq!(initial_seen.len(), 300);
    assert!(initial_frames > 1 && initial_frames < initial_seen.len());
    // 同一组属性接近报文上限；必须按真实字节数拆批，不能把整个组误报为不可发布。
    let large = vec![0x5a; 4000];
    let mut fields = base(false, true);
    fields.push(Attribute { flags: 0xc0, code: 99, data: &large });
    let encoded = attributes(&fields);
    for target in announced.as_chunks::<5>().0 {
        let bytes = update(&[], &encoded, target);
        domain.receive(1, &receive(&mut session, &bytes)).unwrap();
    }
    choose(&mut domain);
    // 普通工作已超过批量额度时，显式刷新仍须推进，不能等普通队列永远排空。
    for (prefix, _) in domain.selected().iter() {
        output.changed(prefix, 1).unwrap();
    }
    // 经实际 Session 的方向授权和族过滤生成刷新副作用；reserved 按 RFC 2918 忽略。
    let mut request = [0; 23];
    request[19..].copy_from_slice(&[0, 1, 91, 1]);
    packet_wire::bgp::emit(&mut request, Kind::RouteRefresh, false).unwrap();
    for effect in session.on(1, Input::Frame(Frame::parse(&request, false).unwrap())) {
        assert_eq!(effect, Effect::Refresh { family: packet_wire::bgp::nlri::Family::Ipv4, enhanced: false });
        output.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut frames = 0;
    let mut announcements = 0;
    let first = output
        .prepare(&mut domain, Some(&policy()), context, 1)
        .unwrap()
        .expect("ordinary backlog must not starve refresh");
    seen.extend(
        Update::parse(&first[19..])
            .unwrap()
            .announced()
            .unwrap()
            .prefixes(),
    );
    announcements += Update::parse(&first[19..]).unwrap().announced().unwrap().prefixes().count();
    output.sent(1);
    frames += 1;
    output.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    output.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    while output.next_deadline().is_some() {
        if let Some(bytes) = output
            .prepare(&mut domain, Some(&policy()), context, 1)
            .unwrap()
        {
            assert!(bytes.len() <= packet_wire::bgp::STANDARD_LIMIT);
            receive(&mut remote, &bytes);
            let decoded = Update::parse(&bytes[19..]).unwrap();
            assert!(decoded.attributes().unwrap().any(|attribute| attribute.code == 99 && attribute.data == large));
            announcements += decoded.announced().unwrap().prefixes().count();
            seen.extend(
                Update::parse(&bytes[19..])
                    .unwrap()
                    .announced()
                    .unwrap()
                    .prefixes(),
            );
            output.sent(1);
            frames += 1;
        }
    }
    assert_eq!(seen, initial_seen);
    assert!(frames > 1 && frames < seen.len());
    assert!(announcements > seen.len()); // 新请求触发重发，普通属性去重不能吞掉它。
    assert_eq!(output.len(), 300);
    let mut empty = AdjRibOut::new();
    empty.refresh(packet_wire::bgp::nlri::Family::Ipv4, false);
    assert!(
        empty
            .prepare(&mut domain, None, context, 2)
            .unwrap()
            .is_none()
    );
    assert_eq!(empty.next_deadline(), None);
}

// 普通重发用例无法证明刷新边界、MRAI 与对端撤回顺序。使用两个真实路由 owner 验证遗漏路由的命运。
#[test]
fn enhanced_refresh_preserves_replayed_routes_and_withdraws_omissions_after_eorr() {
    use bgp::{Effect, Input};
    use packet_wire::bgp::{self as wire, Frame, Kind, nlri::Family};

    let (mut source, mut neighbor) = domain();
    assert!(!neighbor.can_request_refresh(Family::Ipv4));
    learn(&mut source, &mut neighbor, &[]);
    let mut advertised = AdjRibOut::new();
    advertised.changed(prefix(), 0).unwrap();
    let context = context();
    let policy = policy();
    let initial = advertised
        .prepare(&mut source, Some(&policy), context, 0)
        .unwrap()
        .unwrap();
    advertised.sent(0);

    let mut peer = receiver(context);
    assert!(peer.negotiated().unwrap().enhanced_refresh);
    let mut target = RoutingDomain::try_new(
        SelectionContext::new(context.target.asn, context.target.identifier, None, None).unwrap(),
    )
    .unwrap();
    target
        .add_peer(
            Peer {
                id: 1,
                address: context.local_address,
                asn: context.local_as,
                identifier: "192.0.2.9".parse().unwrap(),
                route_reflector_client: false,
                kind: PeerKind::External,
            },
            SourcePeer {
                preference: NonZeroU32::new(20).unwrap(),
                next_hop_interface: None,
            },
        )
        .unwrap();
    target.receive(1, &receive(&mut peer, &initial)).unwrap();
    // 模拟对端先前多留的一条路由。发送方没有这条记录，重发时也不会显式撤回它。
    let mut extra = initial.clone();
    assert!(extra.ends_with(&[24, 198, 51, 100]));
    *extra.last_mut().unwrap() = 101;
    target.receive(1, &receive(&mut peer, &extra)).unwrap();
    let omitted = packet_wire::bgp::nlri::Prefix {
        address: "198.51.101.0".parse().unwrap(),
        length: 24,
    };
    choose(&mut target);
    assert!(target.selected().get(omitted).is_some());

    let boundary = |peer: &mut bgp::Session, target: &mut RoutingDomain, bytes: &[u8], start| {
        let frame = Frame::parse(bytes, false).unwrap();
        assert_eq!(frame.header().kind(), Kind::RouteRefresh);
        let mut effects = peer.on(4, Input::Frame(frame)).into_iter();
        let Some(Effect::RefreshBoundary { family, begin }) = effects.next() else {
            panic!("expected an enhanced refresh boundary");
        };
        assert!(effects.next().is_none());
        assert_eq!((family, begin), (Family::Ipv4, start));
        target.refresh_boundary(1, family, begin).unwrap();
    };
    boundary(
        &mut peer,
        &mut target,
        &wire::route_refresh(Family::Ipv4, 2),
        false,
    );
    assert!(target.pending().is_empty());

    assert!(peer.can_request_refresh(Family::Ipv4));
    assert!(!peer.can_request_refresh(Family::Ipv6));
    let mut requester = AdjRibOut::new();
    requester.request_refresh(Family::Ipv4);
    assert_eq!(requester.next_deadline(), Some(0));
    let request = requester.prepare(&mut target, None, context, 0).unwrap().unwrap();
    assert_eq!(request, wire::route_refresh(Family::Ipv4, 0));
    assert_eq!(requester.prepared_kind(), Some(Kind::RouteRefresh));
    requester.request_refresh(Family::Ipv4);
    assert!(requester.prepare(&mut target, None, context, 0).unwrap().is_none());
    requester.sent(0);
    assert!(requester.next_deadline().is_none());
    assert!(
        peer.on(4, Input::Frame(Frame::parse(&request, false).unwrap()))
            .into_iter()
            .eq([Effect::Refresh {
                family: Family::Ipv4,
                enhanced: true
            }])
    );
    advertised.refresh(Family::Ipv4, true);
    let begin = advertised
        .prepare(&mut source, Some(&policy), context, 1)
        .unwrap()
        .unwrap();
    assert_eq!(advertised.prepared_kind(), Some(Kind::RouteRefresh));
    assert!(
        advertised
            .prepare(&mut source, Some(&policy), context, 1)
            .unwrap()
            .is_none()
    );
    boundary(&mut peer, &mut target, &begin, true);
    advertised.sent(1);
    assert!(target.selected().get(omitted).is_some());
    assert!(
        advertised
            .prepare(&mut source, Some(&policy), context, 1)
            .unwrap()
            .is_none()
    );
    assert_eq!(advertised.next_deadline(), Some(30));

    let replay = advertised
        .prepare(&mut source, Some(&policy), context, 30)
        .unwrap()
        .unwrap();
    assert_eq!(advertised.prepared_kind(), Some(Kind::Update));
    target.receive(1, &receive(&mut peer, &replay)).unwrap();
    assert!(
        target.pending().is_empty(),
        "an identical UPDATE clears staleness without another selection"
    );
    advertised.sent(30);
    let end = advertised
        .prepare(&mut source, Some(&policy), context, 30)
        .unwrap()
        .unwrap();
    boundary(&mut peer, &mut target, &end, false);
    advertised.sent(30);
    assert_eq!(target.pending(), &[omitted]);
    choose(&mut target);
    assert!(target.selected().get(omitted).is_none());
    assert!(target.selected().get(prefix()).is_some());
    assert_eq!(advertised.next_deadline(), None);

    let unknown = wire::route_refresh(Family::Ipv4, 99);
    assert!(
        peer.on(4, Input::Frame(Frame::parse(&unknown, false).unwrap()))
            .into_iter()
            .next()
            .is_none()
    );
    let mut truncated = wire::route_refresh(Family::Ipv4, 2);
    let truncated = &mut truncated[..22];
    wire::emit(truncated, Kind::RouteRefresh, false).unwrap();
    assert!(
        matches!(peer.on(4, Input::Frame(Frame::parse(truncated, false).unwrap())).into_iter().next(),
        Some(Effect::Close { notice: Some(bgp::Notice { code: 7, subcode: 1, data }), withdraw_routes: true, .. }) if data == truncated)
    );
}


#[test]
fn configured_networks_follow_exact_rib_routes_and_use_the_normal_export_pipeline() {
    use router_protocol::{routing as rib, tracking::{self, IpPrefix, Query}};
    let configuration = router_protocol::bgp::Configuration::parse(br#"{
      "local_as":65000,"identifier":"192.0.2.1",
      "networks":[{"network":"203.0.113.0","length":24},{"network":"198.51.100.0","length":24},{"network":"0.0.0.0","length":0},{"network":"198.51.100.0","length":24}],
      "neighbors":{"peer":{"address":"192.0.2.3","asn":65002,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":true,"export":{"default":"permit"}}}
    }"#).unwrap();
    assert_eq!(configuration.networks.len(), 3);
    assert_eq!(router_protocol::bgp::Configuration::parse(&configuration.encode().unwrap()).unwrap(), configuration);
    let mut domain = RoutingDomain::try_new(SelectionContext::new(65000, configuration.identifier, None, None).unwrap()).unwrap();
    let queries = domain.configure_networks(configuration.networks.clone()).unwrap();
    assert_eq!(domain.originated_len(), 0);
    assert!(domain.announcement_prefixes().next().is_none());
    // 同一生产 RIB 的精确观察：父/子网段不能冒充 network 声明，默认网段也必须真实存在。
    let compile = |rows: &[rib::Route]| {
        let immediate = [rib::Member { target: rib::Target::Direct { interface_index: 2, gateway: None, on_link: false }, weight: 1 }];
        let hops = [rib::NextHop::new(rib::Action::Forward, &immediate), rib::NextHop::new(rib::Action::Discard, &[])];
        let bytes = rib::Batch::encode(rib::Family::Ipv4, &[rib::Attributes { preference: 1, metric: 0 }], hops, rows).unwrap();
        let source = routing_rib::Source::<u32, (), routing_rib::NextHop>::capture_batch(10, 10, (), rib::Batch::parse(&bytes).unwrap()).unwrap();
        routing_rib::Catalogue::build(vec![source]).unwrap().select_resolved(|_| true, |_| true).unwrap()
    };
    let row = |address: &str, length, next_hop| rib::Route { network: address.parse().unwrap(), length, attributes: 0, next_hop };
    let table = compile(&[row("0.0.0.0", 0, 0), row("198.51.100.0", 24, 0), row("198.51.100.0", 25, 1), row("203.0.113.0", 25, 0)]);
    let snapshot = |table: &routing_rib::resolution::Resolved<u32, ()>, queries: &[Query]| {
        tracking::OwnedSnapshot::capture(tracking::Snapshot::encode(1, queries.iter().map(|query| table.track(*query))).unwrap()).unwrap()
    };
    let report = snapshot(&table, &queries);
    let changed = domain.update_networks(report.view()).unwrap();
    assert_eq!(domain.originated_len(), 2);
    assert!(domain.selected().is_empty(), "network intent must not create a learned forwarding source");
    assert_eq!(domain.network_is_available(prefix()), Some(true));
    let child_only = packet_wire::bgp::nlri::Prefix { address: "203.0.113.0".parse().unwrap(), length: 24 };
    assert_eq!(domain.network_is_available(child_only), Some(false));
    let mut wrong = queries.clone(); wrong.swap(1, 2);
    assert!(domain.update_networks(snapshot(&table, &wrong).view()).is_err());
    assert_eq!(domain.originated_len(), 2, "invalid batch must retain all prior qualification");
    let policy = configuration.neighbors.values().next().unwrap().export.as_ref().unwrap();
    let target = ExportContext { local_as: 65000, interval_ns: 0, ..context() };
    let mut outgoing = AdjRibOut::new();
    for prefix in &changed { outgoing.changed(*prefix, 0).unwrap(); }
    let bytes = outgoing.prepare(&mut domain, Some(policy), target, 0).unwrap().unwrap();
    let mut remote = receiver(target);
    let received = receive(&mut remote, &bytes);
    assert!(received.path().unwrap().segments().flat_map(|segment| segment.asns()).eq([65000]));
    let decoded = Update::parse(&bytes[19..]).unwrap();
    assert_eq!(decoded.announced().unwrap().prefixes().count(), 2);
    assert!(decoded.attributes().unwrap().any(|attribute| attribute.code == 3 && attribute.data == match target.local_address { core::net::IpAddr::V4(address)=>address.octets(),_=>unreachable!("IPv4 fixture") }));
    assert!(outgoing.is_empty()); assert!(!outgoing.is_advertised(prefix()));
    outgoing.sent(1); assert_eq!(outgoing.len(), 2); assert!(outgoing.is_advertised(prefix()));
    // iBGP 本机宣告也用实际本机地址，不触发分割水平或伪造反射属性。
    let internal = ExportContext { target: Peer { kind: PeerKind::Internal, asn: 65000, ..target.target }, ..target };
    let mut internal_output = AdjRibOut::new();
    for prefix in domain.announcement_prefixes() { internal_output.changed(prefix, 0).unwrap(); }
    let bytes = internal_output.prepare(&mut domain, Some(policy), internal, 0).unwrap().unwrap();
    let mut internal_remote = receiver(internal);
    let received = receive(&mut internal_remote, &bytes);
    assert_eq!(received.path().unwrap().hop_count(), 0);
    let attributes = Update::parse(&bytes[19..]).unwrap().attributes().unwrap().collect::<Vec<_>>();
    assert!(attributes.iter().any(|attribute| attribute.code == 5 && attribute.data == 100u32.to_be_bytes()));
    assert!(!attributes.iter().any(|attribute| matches!(attribute.code, 9 | 10)));
    internal_output.sent(1);
    // 两个子网完整覆盖 /24 也不能代替精确路由，父路由同样不能；失去证明沿同一队列撤回。
    let missing = compile(&[row("198.51.0.0", 16, 0), row("198.51.100.0", 25, 0), row("198.51.100.128", 25, 0), row("203.0.113.0", 25, 0)]);
    let changed = domain.update_networks(snapshot(&missing, &queries).view()).unwrap();
    assert_eq!(domain.originated_len(), 0);
    for prefix in changed { outgoing.changed(prefix, 2).unwrap(); }
    let bytes = outgoing.prepare(&mut domain, Some(policy), target, 2).unwrap().unwrap();
    let decoded = Update::parse(&bytes[19..]).unwrap();
    assert_eq!(decoded.withdrawn().unwrap().prefixes().count(), 2);
    assert_eq!(decoded.announced().unwrap().prefixes().count(), 0);
    outgoing.sent(3); assert!(outgoing.is_empty()); assert!(!outgoing.is_advertised(prefix()));
    // 显式 discard 汇总路由有意承载 network 宣告，不要求为它伪造立即出口。
    let discard = compile(&[row("198.51.100.0", 24, 1)]);
    let changed = domain.update_networks(snapshot(&discard, &queries).view()).unwrap();
    for prefix in changed { outgoing.changed(prefix, 4).unwrap(); }
    assert!(outgoing.prepare(&mut domain, Some(policy), target, 4).unwrap().is_some()); outgoing.sent(5);
    // 同前缀仍可保留真实学习路径；本机资格消失时回到该路径，而非撤销仍可发布的目标。
    domain.add_peer(Peer { id: 1, address: "192.0.2.2".parse().unwrap(), identifier: "192.0.2.2".parse().unwrap(), asn: 65001, kind: PeerKind::External, route_reflector_client: false }, SourcePeer { preference: NonZeroU32::new(20).unwrap(), next_hop_interface: None }).unwrap();
    let mut upstream = bgp_peer::established(false, true);
    learn(&mut domain, &mut upstream, &[]);
    assert!(domain.export_route(prefix()).unwrap().peer.is_none());
    assert_eq!(domain.announcement_prefixes().filter(|candidate| *candidate == prefix()).count(), 1);
    let changed = domain.update_networks(snapshot(&missing, &queries).view()).unwrap();
    for prefix in changed { outgoing.changed(prefix, 6).unwrap(); }
    let bytes = outgoing.prepare(&mut domain, Some(policy), target, 6).unwrap().unwrap();
    let received = receive(&mut remote, &bytes);
    assert!(received.path().unwrap().segments().flat_map(|segment| segment.asns()).eq([65000, 65001]));
    outgoing.sent(7); assert_eq!(outgoing.len(), 1);
    assert!(IpPrefix { network: "0.0.0.0".parse().unwrap(), length: 0 }.valid());
}
