// 静态来源不能证明 MP-BGP 的选择、安装后递归证明、真实 UDP 及撤回闭环；复用同一 Session/RIB/NET 夹具。
#[test]
fn bgp6_learning_installs_a_proven_route_sends_udp_and_withdraws_the_same_family() {
    use crate::bgp_peer::{attributes, base, established, receive, update};
    use ::bgp::{Peer, PeerKind, Reachability, RoutingDomain, SelectionContext, SourcePeer};
    use packet_wire::bgp::{
        attributes::{Attribute, MpReach},
        nlri::{Family, Nlri, Prefix},
    };
    use router_protocol::tracking::{Query, State as RouteState, installation::Installation};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, true)];
    let mut rib = routing_rib::sources::Routes::default();
    let source = rib.reserve_source().unwrap();
    let prepared = rib
        .prepare_configuration(&routing_facts(&net), &[plan(2, &addresses)], &[], None, None)
        .unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
    let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let rib = prepared.next;
    advance_configuration(&net, 2002);
    frames(&device);
    let parameters = router_protocol::bgp::Configuration::parse(br#"{
      "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
        "address":"2001:db8:1::2","asn":65001,"preference":20,"hold_seconds":90,"retry_seconds":5,"passive":false,
        "ipv6_unicast":true,"import":{"default":"permit","preference":100},
        "export":{"default":"permit"}
      },"scoped":{"address":"fe80::3","interface":"eth0","asn":65002,"preference":20,
        "hold_seconds":90,"retry_seconds":5,"passive":true,"ipv6_unicast":true,
        "export":{"default":"permit"}
      }}}"#).unwrap();
    let neighbor = parameters.neighbors.values().next().unwrap();
    let mut routes = RoutingDomain::try_new(
        SelectionContext::new(65000, parameters.identifier, None, None).unwrap(), [false; 2])
    .unwrap();
    let facts = routing_facts(&net);
    let export_addresses: Vec<_> = (0..facts.addresses.len()).filter_map(|index| {
        let row = facts.addresses.get(index).unwrap();
        let core::net::IpAddr::V6(address) = row.address else { return None; };
        if row.interface_index != 2 { return None; }
        assert!(row.usable);
        Some(::bgp::ExportAddress { interface: core::num::NonZeroU32::new(2).unwrap(), address,
            prefix_length: row.prefix_length, on_link: row.on_link })
    }).collect();
    assert!(routes.observe_export_network(export_addresses.iter().copied()).unwrap());
    assert!(!routes.observe_export_network(export_addresses.iter().copied()).unwrap());
    let peer = Peer {
        id: 1,
        address: neighbor.address,
        identifier: "192.0.2.2".parse().unwrap(),
        asn: 65001,
        kind: PeerKind::External,
        route_reflector_client: false,
    };
    routes
        .add_peer(
            peer,
            SourcePeer {
                preference: neighbor.preference,
                next_hop_interface: None,
            },
        )
        .unwrap();
    let prefix = Prefix {
        address: "2001:db8:a00::".parse().unwrap(),
        length: 64,
    };
    // 25 个 /64 的 225-byte NLRI 在附加链路地址后才令 MP 正文越过 255，保护真实长度/容量计算。
    let prefixes: [Prefix; 25] = core::array::from_fn(|index| Prefix {
        address: Ipv6Addr::new(0x2001, 0xdb8, 0xa00 + index as u16, 0, 0, 0, 0, 0).into(),
        length: 64,
    });
    let mut nlri = [0; 25 * 17];
    let length = Nlri::emit(&mut nlri, Family::Ipv6, &prefixes).unwrap();
    let mut reach = [0; 512];
    let mut source_hop = [0; 32];
    source_hop[..16].copy_from_slice(&GLOBAL_PEER.octets());
    source_hop[16..].copy_from_slice(&PEER.octets());
    let length = MpReach::emit(
        &mut reach,
        1,
        &source_hop,
        Nlri::parse(&nlri[..length], Family::Ipv6).unwrap(),
    )
    .unwrap();
    let mut list = base(false, true);
    list.retain(|attribute| attribute.code != 3);
    list.push(Attribute {
        flags: 0x80,
        code: 14,
        data: &reach[..length],
    });
    let input = update(&[], &attributes(&list), &[]);
    let mut session = established(false, true);
    routes
        .receive(peer.id, &receive(&mut session, &input))
        .unwrap();
    let raw = routes.peer_routes(peer.id).unwrap().get(prefix, None).unwrap();
    let decision = ::bgp::import_route(neighbor.import.as_ref(), prefix, &peer, raw).unwrap();
    let bytes = routes
        .propose(Family::Ipv6, |_, _, _, route| {
            Ok((
                decision.next_hop,
                Some(decision.preference),
                route.attributes(),
            ))
        })
        .unwrap()
        .encode_source(neighbor.preference)
        .unwrap();
    let captured = routing_rib::sources::Routes::capture_source(
        source,
        source,
        router_protocol::bgp::ROUTE_PROTOCOL,
        20,
        router_protocol::routing::Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    let prepared = rib
        .prepare_sources(&routing_facts(&net), Some(captured), &[])
        .unwrap();
    let probe = Installation {
        interface: None,
        network: prefix.address,
        length: prefix.length,
        next_hop: Query {
            address: GLOBAL_PEER.into(),
            resolve_via_default: false,
            prefix_length: None,
        },
    };
    let snapshot = prepared.next.snapshot().unwrap().clone();
    // 同批交错两族，且 IPv6 候选覆盖其下一跳时必须失败，不能退到旧直连祖先。
    let cuts = [
        probe,
        Installation {
            interface: None,
            network: "203.0.113.0".parse().unwrap(),
            length: 24,
            next_hop: Query {
                address: "127.0.0.2".parse().unwrap(),
                ..probe.next_hop
            },
        },
        Installation {
            network: "2001:db8:1::".parse().unwrap(),
            length: 64,
            ..probe
        },
        Installation {
            network: GLOBAL_PEER.into(),
            length: 128,
            ..probe
        },
        probe,
    ];
    let bytes = snapshot.evaluate_installations(source, &cuts).unwrap();
    let outcomes = router_protocol::tracking::installation::decode_outcomes(
        &bytes,
        source,
        snapshot.generation(),
        cuts.len(),
    )
    .unwrap();
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.reachable)
            .collect::<Vec<_>>(),
        [true, true, false, false, true]
    );
    let outcome = outcomes[0];
    assert!(outcome.reachable);
    let selected = routes
        .prepare(Family::Ipv6, |_, _, _, route| {
            Ok((
                decision.next_hop,
                Some(decision.preference),
                Reachability::Resolved {
                    interior_cost: outcome.interior_cost,
                },
                route.attributes(),
            ))
        })
        .unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
    let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    selected.commit();
    let query = Query {
        address: "2001:db8:a00::9".parse().unwrap(),
        resolve_via_default: false,
        prefix_length: None,
    };
    let snapshot = prepared.next.snapshot().unwrap().clone();
    assert_eq!(snapshot.lookup(query).state, RouteState::Forward);
    assert_eq!(snapshot.lookup(query).route.unwrap().source, source);
    let frozen =
        router_protocol::tracking::OwnedSnapshot::capture(snapshot.encode(&[query]).unwrap())
            .unwrap();
    assert_eq!(
        frozen
            .view()
            .records()
            .next()
            .unwrap()
            .members()
            .next()
            .unwrap()
            .target
            .gateway,
        Some(GLOBAL_PEER.into())
    );
    let endpoint = socket6(&net);
    endpoint
        .connect(InetAddress::new(query.address, 44000))
        .unwrap();
    endpoint.send_to(b"bgp6", None, &[]).unwrap();
    poll(&net, 4004);
    poll(&net, 4004);
    let output = frames(&device);
    assert!(output.iter().any(|frame| matches!(
        NdPacket::parse(parse(frame).1).unwrap().message(),
        NdMessage::NeighborSolicitation {
            target: GLOBAL_PEER
        }
    )));
    enqueue(&device, request(GLOBAL_PEER, GLOBAL));
    poll(&net, 4005);
    let output = frames(&device);
    assert!(
        output
            .iter()
            .any(
                |frame| Ipv6Packet::parse(&frame[14..]).is_ok_and(|ip| ip.upper_protocol() == 17
                    && ip.destination()
                        == match query.address {
                            core::net::IpAddr::V6(address) => address,
                            _ => unreachable!(),
                        })
            )
    );
    let target = ::bgp::ExportContext {
        local_as: 65000,
        local_address: GLOBAL.into(),
        ipv6_unicast: true,
        ipv4_unicast: true,
        send_ipv4_via_ipv6: false,
        cluster_id: None,
        target: Peer {
            id: 2,
            asn: 65002,
            address: "2001:db8:1::3".parse().unwrap(),
            identifier: "192.0.2.3".parse().unwrap(),
            kind: PeerKind::External,
            route_reflector_client: false,
        },
        width: packet_wire::bgp::attributes::AsnWidth::Four,
        interval_ns: 0,
    };
    let mut output = ::bgp::AdjRibOut::default();
    for prefix in prefixes {
        output.changed(&routes, prefix, 0).unwrap();
    }
    let bytes = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 0)
        .unwrap()
        .unwrap();
    let mut receiver = established_with_receiver();
    let received = receive(&mut receiver, &bytes);
    assert_eq!(received.changes().count(), prefixes.len());
    assert!(
        matches!(received.changes().next(), Some(::bgp::Change::Announce {prefix:actual, path_id: None, next_hop}) if actual == prefix && next_hop.address == GLOBAL && next_hop.link_local == Some(LOCAL))
    );
    let mp = packet_wire::bgp::update::Update::parse(&bytes[19..]).unwrap().attributes().unwrap().find(|attribute| attribute.code == 14).unwrap();
    assert_eq!(mp.data.len(), 37 + prefixes.len() * 9);
    assert_ne!(mp.flags & 0x10, 0);
    output.sent(0);
    output.refresh(Family::Ipv4, false);
    assert!(
        output
            .prepare(&mut routes, neighbor.export.as_ref(), target, 1)
            .unwrap()
            .is_none()
    );
    output.refresh(Family::Ipv6, true);
    let start = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 1)
        .unwrap()
        .unwrap();
    assert_eq!(start, packet_wire::bgp::route_refresh(Family::Ipv6, 1));
    output.sent(1);
    let bytes = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        receive(&mut receiver, &bytes).changes().count(),
        prefixes.len()
    );
    output.sent(1);
    let end = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 1)
        .unwrap()
        .unwrap();
    assert_eq!(end, packet_wire::bgp::route_refresh(Family::Ipv6, 2));
    output.sent(1);
    // 链路 TCP 根据实际范围选择本机全局下一跳，不能借另一个接口的 fe80 地址证明共链路。
    let scoped = parameters.neighbors.iter().find(|(name, _)| name.as_str() == "scoped").unwrap().1;
    let scoped_peer = Peer { id: 3, address: scoped.address, asn: scoped.asn.get(), ..target.target };
    routes.add_peer(scoped_peer, SourcePeer { preference: scoped.preference,
        next_hop_interface: core::num::NonZeroU32::new(2) }).unwrap();
    let scoped_context = ::bgp::ExportContext { target: scoped_peer, local_address: LOCAL.into(), ..target };
    let mut scoped_output = ::bgp::AdjRibOut::default();
    scoped_output.changed(&routes, prefix, 2).unwrap();
    let bytes = scoped_output.prepare(&mut routes, scoped.export.as_ref(), scoped_context, 2).unwrap().unwrap();
    assert!(matches!(receive(&mut receiver, &bytes).changes().next(), Some(::bgp::Change::Announce {next_hop,..}) if next_hop.address == GLOBAL && next_hop.link_local == Some(LOCAL)));
    let wrong_scope = Peer { id: 4, ..scoped_peer };
    routes.add_peer(wrong_scope, SourcePeer { preference: scoped.preference,
        next_hop_interface: core::num::NonZeroU32::new(77) }).unwrap();
    let mut invalid_scope = ::bgp::AdjRibOut::default();
    invalid_scope.changed(&routes, prefix, 2).unwrap();
    assert!(invalid_scope.prepare(&mut routes, scoped.export.as_ref(), ::bgp::ExportContext {
        target: wrong_scope, ..scoped_context
    }, 2).unwrap().is_none());
    assert_eq!(invalid_scope.rejected_route(), Some((prefix, packet_wire::PacketError::Unsupported)));
    // 同链路的第三方下一跳保留它自己的辅助地址；不能把本机 LOCAL 填到别的路由器名下。
    let internal = ::bgp::ExportContext {
        target: Peer { asn: 65000, kind: PeerKind::Internal, ..target.target },
        ..target
    };
    let mut internal_session = crate::bgp_peer::established_with(::bgp::Config {
        local_as: 65000, peer_as: 65000, ..crate::bgp_peer::config(true)
    }, true);
    let mut third_party = ::bgp::AdjRibOut::default();
    third_party.changed(&routes, prefix, 2).unwrap();
    let bytes = third_party.prepare(&mut routes, neighbor.export.as_ref(), internal, 2).unwrap().unwrap();
    assert!(matches!(receive(&mut internal_session, &bytes).changes().next(), Some(::bgp::Change::Announce {next_hop,..}) if next_hop.address == GLOBAL_PEER && next_hop.link_local == Some(PEER)));
    // 发往另一个链路的邻居只携全局地址；不传播没有同链路意义的上游辅助地址。
    let remote_target = ::bgp::ExportContext { target: Peer {
        address: "2001:db8:77::2".parse().unwrap(), ..internal.target
    }, ..internal };
    let mut remote_output = ::bgp::AdjRibOut::default();
    remote_output.changed(&routes, prefix, 2).unwrap();
    let bytes = remote_output.prepare(&mut routes, neighbor.export.as_ref(), remote_target, 2).unwrap().unwrap();
    assert!(matches!(receive(&mut internal_session, &bytes).changes().next(), Some(::bgp::Change::Announce {next_hop,..}) if next_hop.address == GLOBAL_PEER && next_hop.link_local.is_none()));
    // 辅助地址单独变化也要 UPDATE，不能只比较全局地址；接收端看到的值直接来自生产 codec。
    let replacement = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 9);
    let moved = export_addresses.iter().copied().map(|mut row| {
        if row.address == LOCAL { row.address = replacement; }
        row
    });
    assert!(routes.observe_export_network(moved).unwrap());
    output.reconsider(Family::Ipv6);
    let bytes = output.prepare(&mut routes, neighbor.export.as_ref(), target, 2).unwrap().unwrap();
    assert!(matches!(receive(&mut receiver, &bytes).changes().next(), Some(::bgp::Change::Announce { next_hop, .. }) if next_hop.link_local == Some(replacement)));
    output.sent(2);
    // DAD 未完成不能发布，随后资格恢复必须能再次宣告，而不是把先前拒绝永远留在表外。
    assert!(routes.observe_export_network(export_addresses.iter().copied().filter(|row| row.address != LOCAL)).unwrap());
    output.reconsider(Family::Ipv6);
    let bytes = output.prepare(&mut routes, neighbor.export.as_ref(), target, 3).unwrap().unwrap();
    assert_eq!(receive(&mut receiver, &bytes).changes().next(), Some(::bgp::Change::Withdraw(prefix, None)));
    output.sent(3);
    assert!(routes.observe_export_network(export_addresses.iter().copied()).unwrap());
    output.reconsider(Family::Ipv6);
    let bytes = output.prepare(&mut routes, neighbor.export.as_ref(), target, 4).unwrap().unwrap();
    assert!(matches!(receive(&mut receiver, &bytes).changes().next(), Some(::bgp::Change::Announce {next_hop,..}) if next_hop.link_local == Some(LOCAL)));
    output.sent(4);
    output.reconsider(Family::Ipv6);
    assert!(output.prepare(&mut routes, neighbor.export.as_ref(), target, 4).unwrap().is_none());
    assert!(output.next_deadline().is_none());
    let rib = prepared.next;
    routes.remove_peer(peer.id).unwrap();
    let selected = routes
        .prepare(Family::Ipv6, |_, _, _, _| unreachable!("removed peer"))
        .unwrap();
    let bytes = selected.encode_source(neighbor.preference).unwrap();
    let withdrawn = routing_rib::sources::Routes::capture_source(
        source,
        source,
        router_protocol::bgp::ROUTE_PROTOCOL,
        20,
        router_protocol::routing::Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    let prepared = rib
        .prepare_sources(&routing_facts(&net), Some(withdrawn), &[])
        .unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
    let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    selected.commit();
    assert_eq!(
        prepared.next.snapshot().unwrap().lookup(query).state,
        RouteState::NoRoute
    );
    assert!(snapshot.changes_from(prepared.next.snapshot().unwrap(), &[query]));
    assert_eq!(
        endpoint.send_to(b"withdrawn", None, &[]),
        Err(SocketError::NetworkUnreachable)
    );
    for prefix in prefixes {
        output.changed(&routes, prefix, 5).unwrap();
    }
    let bytes = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 5)
        .unwrap()
        .unwrap();
    let withdrawn = receive(&mut receiver, &bytes);
    assert_eq!(withdrawn.changes().count(), prefixes.len());
    assert_eq!(
        withdrawn.changes().next(),
        Some(::bgp::Change::Withdraw(prefix, None))
    );
    output.sent(5);
    output.finish_initial(Family::Ipv6);
    let bytes = output
        .prepare(&mut routes, neighbor.export.as_ref(), target, 6)
        .unwrap()
        .unwrap();
    assert_eq!(
        receive(&mut receiver, &bytes).end_of_rib(),
        Some(Family::Ipv6)
    );
}
fn established_with_receiver() -> ::bgp::Session {
    crate::bgp_peer::established_with(
        ::bgp::Config {
            local_as: 65002,
            peer_as: 65000,
            ..crate::bgp_peer::config(false)
        },
        true,
    )
}

// 现有 IPv6 单播用例无法发现 IPv4 MP 下一跳/撤回用错族，或安装时证明了与实际出口不同的地址。
#[test]
fn bgp_ipv4_via_ipv6_uses_the_negotiated_hop_for_forwarding_and_withdrawal() {
    use crate::bgp_peer::{attributes, base, receive, update};
    use ::bgp::{
        AdjRibOut, Change, ExportContext, Peer, PeerKind, Reachability, RoutingDomain,
        SelectionContext, SourcePeer,
    };
    use core::num::NonZeroU32;
    use packet_wire::{
        bgp::{
            attributes::{AsnWidth, Attribute, MpReach, MpUnreach},
            nlri::{Family, Nlri, Prefix},
            update::Update,
        },
        ipv4::Ipv4Packet,
    };
    use router_protocol::tracking::{Query, State as RouteState, installation};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, device) = routing_domain(89254);
    configure_address(&net, 3, Ipv4Addr::UNSPECIFIED).unwrap();
    ready(&net, &device);
    let parameters = router_protocol::bgp::Configuration::parse(
        br#"{
        "local_as":65000,"identifier":"192.0.2.1","neighbors":{"peer":{
            "address":"fe80::2","interface":"eth1","asn":65001,"preference":20,
            "hold_seconds":90,"retry_seconds":5,"passive":false,"ipv6_unicast":false,
            "import":{"default":"permit","preference":100},"export":{"default":"permit"}
        }}}"#,
    )
    .unwrap();
    let neighbor = parameters.neighbors.values().next().unwrap();
    let mut routes = RoutingDomain::try_new(
        SelectionContext::new(65000, parameters.identifier, None, None).unwrap(), [false; 2])
    .unwrap();
    let peer = Peer {
        id: 1,
        address: neighbor.address,
        identifier: "192.0.2.2".parse().unwrap(),
        asn: 65001,
        kind: PeerKind::External,
        route_reflector_client: false,
    };
    let interface = NonZeroU32::new(3).unwrap();
    routes
        .add_peer(
            peer,
            SourcePeer { preference: neighbor.preference, next_hop_interface: Some(interface) },
        )
        .unwrap();
    let destination = Ipv4Addr::new(203, 0, 113, 9);
    let prefix = Prefix { address: destination.into(), length: 32 };
    let mut encoded_prefix = [0; 5];
    Nlri::emit(&mut encoded_prefix, Family::Ipv4, &[prefix]).unwrap();
    let nlri = Nlri::parse(&encoded_prefix, Family::Ipv4).unwrap();
    let global: Ipv6Addr = "2001:db8:ffff::2".parse().unwrap();
    let mut hop = [0; 32];
    hop[..16].copy_from_slice(&global.octets());
    hop[16..].copy_from_slice(&PEER.octets());
    let mut reach = [0; 42];
    let len = MpReach::emit(&mut reach, 1, &hop, nlri).unwrap();
    let mut list = base(false, true);
    list.retain(|attribute| attribute.code != 3);
    list.insert(0, Attribute { flags: 0x80, code: 14, data: &reach[..len] });
    let input = update(&[], &attributes(&list), &[]);
    let mut session = crate::bgp_peer::established_with(
        ::bgp::Config { ipv6_unicast: false, ..crate::bgp_peer::config(false) },
        true,
    );
    let negotiated = session.negotiated().unwrap();
    assert!(
        negotiated.send_ipv4_via_ipv6
            && negotiated.receive_ipv4_via_ipv6
            && !negotiated.ipv6_unicast
    );
    routes.receive(peer.id, &receive(&mut session, &input)).unwrap();
    let raw = routes.peer_routes(peer.id).unwrap().get(prefix, None).unwrap();
    let decision = ::bgp::import_route(neighbor.import.as_ref(), prefix, &peer, raw).unwrap();
    let probe = routes.installation(prefix, peer.id, decision.next_hop, false).unwrap();
    assert_eq!((probe.interface, probe.next_hop.address), (Some(interface), PEER.into()));
    let mut rib = routing_rib::sources::Routes::default();
    let source = rib.reserve_source().unwrap();
    let capture = |bytes: &[u8]| {
        routing_rib::sources::Routes::capture_source(
            source,
            source,
            router_protocol::bgp::ROUTE_PROTOCOL,
            20,
            router_protocol::routing::Batch::parse(bytes).unwrap(),
        )
        .unwrap()
    };
    let install = |prepared: routing_rib::sources::PreparedRoutes| {
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        prepared.next
    };
    let proposal = routes
        .propose(Family::Ipv4, |_, _, _, route| {
            Ok((decision.next_hop, Some(decision.preference), route.attributes()))
        })
        .unwrap()
        .encode_source(neighbor.preference)
        .unwrap();
    let prepared =
        rib.prepare_sources(&routing_facts(&net), Some(capture(&proposal)), &[]).unwrap();
    let snapshot = prepared.next.snapshot().unwrap();
    let probes = [
        probe,
        installation::Installation { interface: NonZeroU32::new(99), ..probe },
        installation::Installation {
            interface: None,
            next_hop: Query { address: global.into(), ..probe.next_hop },
            ..probe
        },
    ];
    let body = installation::encode(&probes).unwrap();
    let decoded = installation::decode(&body).unwrap();
    assert_eq!(decoded, probes);
    let proof = snapshot.evaluate_installations(source, &decoded).unwrap();
    let verdicts =
        installation::decode_outcomes(&proof, source, snapshot.generation(), probes.len()).unwrap();
    assert!(verdicts.iter().map(|outcome| outcome.reachable).eq([true, false, false]));
    let selection = routes
        .prepare(Family::Ipv4, |_, _, _, route| {
            Ok((
                decision.next_hop,
                Some(decision.preference),
                Reachability::Resolved { interior_cost: verdicts[0].interior_cost },
                route.attributes(),
            ))
        })
        .unwrap();
    assert_eq!(selection.encode_source(neighbor.preference).unwrap(), proposal);
    rib = install(prepared);
    selection.commit();
    let query = Query { address: prefix.address, resolve_via_default: false, prefix_length: None };
    assert_eq!(rib.snapshot().unwrap().lookup(query).state, RouteState::Forward);
    enqueue(&device, request(PEER, LOCAL));
    poll(&net, 3000);
    frames(&device);
    let socket = scoped_endpoint(&net, InetProtocol::Udp);
    let target = InetAddress::new(destination.into(), 44000);
    socket.send_to(b"bgp", Some(target), &[]).unwrap();
    poll(&net, 3001);
    assert!(frames(&device).iter().any(|frame| {
        let ethernet = EthernetFrame::parse(frame).unwrap();
        ethernet.destination() == MAC
            && ethernet.type_or_length() == 0x0800
            && Ipv4Packet::parse(ethernet.payload()).is_ok_and(|ip| ip.destination() == destination)
    }));

    let mut output = AdjRibOut::default();
    let context = ExportContext {
        local_as: parameters.local_as.get(),
        local_address: GLOBAL.into(),
        ipv4_unicast: true,
        ipv6_unicast: false,
        send_ipv4_via_ipv6: negotiated.send_ipv4_via_ipv6,
        cluster_id: None,
        target: Peer {
            id: 2,
            asn: 65002,
            address: "2001:db8:2::3".parse().unwrap(),
            identifier: "192.0.2.3".parse().unwrap(),
            ..peer
        },
        width: AsnWidth::Four,
        interval_ns: 0,
    };
    output.changed(&routes, prefix, 0).unwrap();
    assert!(
        output
            .prepare(
                &mut routes,
                neighbor.export.as_ref(),
                ExportContext { send_ipv4_via_ipv6: false, ..context },
                0
            )
            .unwrap()
            .is_none()
    );
    output.changed(&routes, prefix, 0).unwrap();
    let mut receiver = established_with_receiver();
    let announcement =
        output.prepare(&mut routes, neighbor.export.as_ref(), context, 0).unwrap().unwrap();
    let frame = Update::parse(&announcement[19..]).unwrap();
    assert!(frame.announced().unwrap().bytes().is_empty());
    assert_eq!(frame.scan_attributes().next().unwrap().unwrap().code, 14);
    assert!(receive(&mut receiver, &announcement).changes().eq([Change::Announce {
        prefix,
        path_id: None, next_hop: ::bgp::NextHop { address: GLOBAL.into(), link_local: None },
    }]));
    output.sent(0);

    let mut withdraw = [0; 8];
    let len = MpUnreach::emit(&mut withdraw, 1, nlri).unwrap();
    let input = update(
        &[],
        &attributes(&[Attribute { flags: 0x80, code: 15, data: &withdraw[..len] }]),
        &[],
    );
    routes.receive(peer.id, &receive(&mut session, &input)).unwrap();
    let selection =
        routes.prepare(Family::Ipv4, |_, _, _, _| unreachable!("withdrawn prefix")).unwrap();
    let withdrawal = selection.encode_source(neighbor.preference).unwrap();
    rib = install(
        rib.prepare_sources(&routing_facts(&net), Some(capture(&withdrawal)), &[]).unwrap(),
    );
    selection.commit();
    assert_eq!(rib.snapshot().unwrap().lookup(query).state, RouteState::NoRoute);
    assert_eq!(
        socket.send_to(b"withdrawn", Some(target), &[]),
        Err(SocketError::NetworkUnreachable)
    );
    output.changed(&routes, prefix, 1).unwrap();
    let withdrawal =
        output.prepare(&mut routes, neighbor.export.as_ref(), context, 1).unwrap().unwrap();
    let frame = Update::parse(&withdrawal[19..]).unwrap();
    assert!(frame.withdrawn().unwrap().bytes().is_empty());
    assert_eq!(frame.scan_attributes().next().unwrap().unwrap().code, 15);
    assert!(receive(&mut receiver, &withdrawal).changes().eq([Change::Withdraw(prefix, None)]));
    output.sent(1);
    socket.close_description();
}

// 学习路由用例不能覆盖本机发布资格、DAD 与递归屏障的区别；仅补同一真实 NET/RIB/BGP 路径的状态转换。
#[test]
fn local_host_routes_publish_only_usable_addresses_and_never_prove_recursive_next_hops() {
    use ::bgp::{
        AdjRibOut, Change, ExportContext, Peer, PeerKind, RoutingDomain, SelectionContext,
    };
    use router_protocol::{
        routing,
        tracking::{Query, State as RouteState, installation::Installation},
    };
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let local: Ipv6Addr = "2001:db8:3::1".parse().unwrap();
    let rejected: Ipv6Addr = "2001:db8:dead::1".parse().unwrap();
    let loopback = [definition(local, 128, true)];
    let physical = [definition(GLOBAL, 128, true)];
    let mut rib = routing_rib::sources::Routes::default();
    let source = rib.reserve_source().unwrap();
    let bytes = routing::Batch::encode(
        routing::Family::Ipv6,
        &[routing::Attributes {
            preference: 1,
            metric: 0,
        }],
        [routing::NextHop::new(routing::Action::Prohibit, &[])],
        [routing::Route {
            network: rejected.into(),
            length: 128,
            attributes: 0,
            next_hop: 0,
        }],
    )
    .unwrap();
    let blocked = routing_rib::sources::Routes::capture_source(
        source,
        source,
        4,
        1,
        routing::Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    // 事务仍执行生产 NET：成功确认后才把候选作为当前图交给 BGP；不伪造地址或分发资格。
    let publish = |prepared: routing_rib::sources::PreparedRoutes| {
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        prepared.next
    };
    let rib = publish(
        rib.prepare_configuration(
            &routing_facts(&net),
            &[plan(1, &loopback), plan(2, &physical)],
            &[blocked],
            None,
            None,
        )
        .unwrap(),
    );
    let initial = rib.snapshot().unwrap().clone();
    let exact = |address: Ipv6Addr| Query {
        address: address.into(),
        prefix_length: Some(128),
        resolve_via_default: false,
    };
    assert_eq!(initial.lookup(exact(local)).state, RouteState::Local);
    assert_eq!(initial.lookup(exact(GLOBAL)).state, RouteState::Prohibit);
    assert_eq!(initial.lookup(exact(rejected)).state, RouteState::Prohibit);
    assert_eq!(row(&net, local).unwrap().state, State::Preferred);
    let parameters = router_protocol::bgp::Configuration::parse(br#"{
        "local_as":65000,"identifier":"192.0.2.1",
        "networks":[{"network":"2001:db8:1::abcd","length":128},
                    {"network":"2001:db8:3::1","length":128},
                    {"network":"2001:db8:dead::1","length":128}],
        "neighbors":{"peer":{"address":"192.0.2.3","asn":65002,"preference":20,"hold_seconds":90,
        "retry_seconds":5,"passive":true,"ipv6_unicast":true,
        "export":{"default":"deny","rules":[{"action":{"kind":"permit","next_hop":"2001:db8:1::abcd"}}]}}}
    }"#).unwrap();
    let mut routes = RoutingDomain::try_new(
        SelectionContext::new(65000, parameters.identifier, None, None).unwrap(), [false; 2])
    .unwrap();
    let queries = routes
        .configure_networks(parameters.networks.clone())
        .unwrap();
    let qualify = |routes: &mut RoutingDomain, rib: &routing_rib::sources::Routes| {
        let snapshot = router_protocol::tracking::OwnedSnapshot::capture(
            rib.snapshot().unwrap().encode(&queries).unwrap(),
        )
        .unwrap();
        routes.update_networks(snapshot.view()).unwrap()
    };
    let prefix = |address: Ipv6Addr| packet_wire::bgp::nlri::Prefix {
        address: address.into(),
        length: 128,
    };
    assert_eq!(qualify(&mut routes, &rib), [prefix(local)]);
    assert_eq!(routes.network_is_available(prefix(GLOBAL)), Some(false));
    assert_eq!(routes.network_is_available(prefix(rejected)), Some(false));
    advance_configuration(&net, 2002);
    frames(&device);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    let rib = publish(
        rib.prepare_sources(&routing_facts(&net), None, &[])
            .unwrap(),
    );
    assert_eq!(qualify(&mut routes, &rib), [prefix(GLOBAL)]);
    assert_eq!(
        rib.snapshot().unwrap().lookup(exact(GLOBAL)).state,
        RouteState::Local
    );
    assert_eq!(
        initial.lookup(exact(GLOBAL)).state,
        RouteState::Prohibit,
        "old graph remains frozen"
    );
    let probes = [
        Installation {
            interface: None,
            network: "2001:db8:400::".parse().unwrap(),
            length: 64,
            next_hop: Query {
                address: GLOBAL.into(),
                prefix_length: None,
                resolve_via_default: false,
            },
        },
        Installation {
            interface: None,
            next_hop: Query {
                address: local.into(),
                prefix_length: None,
                resolve_via_default: false,
            },
            network: "2001:db8:500::".parse().unwrap(),
            length: 64,
        },
        Installation {
            interface: None,
            network: "203.0.113.0".parse().unwrap(),
            length: 24,
            next_hop: Query {
                address: "127.0.0.1".parse().unwrap(),
                prefix_length: None,
                resolve_via_default: false,
            },
        },
    ];
    let snapshot = rib.snapshot().unwrap();
    let outcomes = snapshot.evaluate_installations(source, &probes).unwrap();
    assert!(
        router_protocol::tracking::installation::decode_outcomes(
            &outcomes,
            source,
            snapshot.generation(),
            probes.len()
        )
        .unwrap()
        .iter()
        .all(|outcome| !outcome.reachable)
    );
    // 同地址更改前缀/直连意图仍保留真实 DAD 资格，不能在 RIB 中重新变成 tentative。
    let edited = [definition(GLOBAL, 64, false)];
    let prepared = rib
        .prepare_configuration(&routing_facts(&net), &[plan(2, &edited)], &[], None, None)
        .unwrap();
    assert_eq!(
        prepared
            .next
            .snapshot()
            .unwrap()
            .lookup(exact(GLOBAL))
            .state,
        RouteState::Local
    );
    let rib = publish(prepared);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    let actual = route_snapshot::<Ipv6Addr>(&net).unwrap();
    for address in [GLOBAL, local] {
        assert_eq!(
            actual
                .iter()
                .filter(|entry| entry.route.destination == address
                    && entry.route.kind == network_protocol::RouteKind::Local)
                .count(),
            1
        );
    }
    let context = ExportContext {
        local_as: 65000,
        local_address: parameters.identifier.into(),
        ipv4_unicast: true,
        send_ipv4_via_ipv6: false,
        ipv6_unicast: true,
        cluster_id: None,
        target: Peer {
            id: 2,
            asn: 65002,
            address: "192.0.2.3".parse().unwrap(),
            identifier: "192.0.2.3".parse().unwrap(),
            kind: PeerKind::External,
            route_reflector_client: false,
        },
        width: packet_wire::bgp::attributes::AsnWidth::Four,
        interval_ns: 0,
    };
    let policy = parameters
        .neighbors
        .values()
        .next()
        .unwrap()
        .export
        .as_ref();
    let mut output = AdjRibOut::default();
    output.reconsider(packet_wire::bgp::nlri::Family::Ipv6);
    let bytes = output
        .prepare(&mut routes, policy, context, 0)
        .unwrap()
        .unwrap();
    let mut receiver = established_with_receiver();
    let received = crate::bgp_peer::receive(&mut receiver, &bytes);
    assert_eq!(
        received
            .changes()
            .map(|change| match change {
                Change::Announce { prefix, .. } => prefix,
                _ => panic!("local announcement"),
            })
            .collect::<Vec<_>>(),
        [prefix(GLOBAL), prefix(local)]
    );
    output.sent(0);
    assert_eq!(output.len(), 2);
    configure_up(&net, 2, false).unwrap();
    configure_up(&net, 2, true).unwrap();
    poll(&net, 5000);
    let destination = group(GLOBAL);
    let mut conflict = vec![0; 78];
    NdMessage::NeighborSolicitation { target: GLOBAL }
        .emit(&mut conflict[54..], Ipv6Addr::UNSPECIFIED, destination)
        .unwrap();
    envelope(
        &mut conflict,
        Ipv6Addr::UNSPECIFIED,
        destination,
        false,
        255,
    );
    enqueue(&device, conflict);
    poll(&net, 5001);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Duplicate);
    let rib = publish(
        rib.prepare_sources(&routing_facts(&net), None, &[])
            .unwrap(),
    );
    assert_eq!(qualify(&mut routes, &rib), [prefix(GLOBAL)]);
    assert_eq!(
        rib.snapshot().unwrap().lookup(exact(GLOBAL)).state,
        RouteState::Prohibit
    );
    assert_eq!(routes.network_is_available(prefix(rejected)), Some(false));
    output.changed(&routes, prefix(GLOBAL), 1).unwrap();
    let bytes = output
        .prepare(&mut routes, policy, context, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::bgp_peer::receive(&mut receiver, &bytes)
            .changes()
            .collect::<Vec<_>>(),
        [Change::Withdraw(prefix(GLOBAL), None)]
    );
    output.sent(1);
    assert_eq!(output.len(), 1);
    let rib = publish(
        rib.prepare_configuration(&routing_facts(&net), &[plan(1, &[])], &[], None, None)
            .unwrap(),
    );
    assert_eq!(qualify(&mut routes, &rib), [prefix(local)]);
    output.changed(&routes, prefix(local), 2).unwrap();
    let bytes = output
        .prepare(&mut routes, policy, context, 2)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::bgp_peer::receive(&mut receiver, &bytes)
            .changes()
            .collect::<Vec<_>>(),
        [Change::Withdraw(prefix(local), None)]
    );
    output.sent(2);
    assert!(output.is_empty());
}
