// 直连邻居测试无法发现“解析远端而非网关”及换路后误删等待报文；使用同一生产批次与 NIC。
fn gateway_route(gateway: Ipv6Addr) -> network_protocol::RouteRequest<Ipv6Addr> {
    network_protocol::RouteRequest {
        destination: Ipv6Addr::UNSPECIFIED,
        prefix_length: 0,
        table: 254,
        metric: 10,
        scope: 0,
        protocol: 3,
        kind: network_protocol::RouteKind::Unicast,
        interface_index: Some(2),
        gateway: Some(gateway),
        preferred_source: Some(GLOBAL),
        metrics: network_protocol::RouteMetrics::default(),
        on_link: false,
    }
}
fn prepare6(
    net: &Arc<crate::namespace::Namespace>,
    plans: &[InterfaceConfiguration<'_>],
    routes: &[network_protocol::RouteRequest<Ipv6Addr>],
) -> Result<crate::inet::configuration::ConfigurationSession, SocketError> {
    let bytes = network_protocol::ConfigurationBatch::encode_forwarding(
        plans,
        core::iter::empty(),
        core::iter::empty(),
        core::iter::empty(),
        routes
            .iter()
            .copied()
            .map(network_protocol::routes::RouteConfiguration::Inline),
        &[],
        None,
    )
    .unwrap();
    crate::inet::prepare_configuration(
        net,
        Some(network_protocol::ConfigurationBatch::parse(&bytes).unwrap()),
        None,
    )
}
#[test]
fn gateway6_keeps_remote_ip_resolves_gateway_and_preserves_routes_across_interface_changes() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, true)];
    // 用户 JSON、持久化往返、来源编译及实际 RIB 都执行生产路径，不直接构造受管 FIB。
    use router_protocol::configuration::{Configuration, StaticAction, StaticRoute};
    use router_protocol::routing::{self, Family};
    let configuration = Configuration::decode(br#"{
      "version":1,
      "interfaces":{"uplink":{"device":{"kind":"name","value":"eth0"}}},
      "routes":[
        {"destination":"2001:db8:800::","prefix_length":64,"metric":10,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"uplink","gateway":"fe80::2"}}]}},
        {"destination":"2001:db8:900::","prefix_length":64,"metric":10,"action":{"kind":"forward","members":[{"target":{"kind":"recursive","address":"2001:db8:800::1"}}]}},
        {"destination":"2001:db8:dead::","prefix_length":48,"metric":10,"action":{"kind":"discard"}}
      ]
    }"#).unwrap();
    let configuration = Configuration::decode(&configuration.encode().unwrap()).unwrap();
    for (destination, length, gateway) in [
        ("2001:db8:800::", 64, "192.0.2.1"),
        ("2001:db8:800::", 129, "fe80::2"),
        ("192.0.2.0", 64, "192.0.2.1"),
    ] {
        let mut invalid = configuration.clone();
        invalid.routes[0].destination = destination.parse().unwrap();
        invalid.routes[0].prefix_length = length;
        if let StaticAction::Forward { members } = &mut invalid.routes[0].action
            && let router_protocol::configuration::StaticTarget::Direct { gateway: value, .. } =
                &mut members[0].target
        {
            *value = Some(gateway.parse().unwrap());
        }
        assert!(invalid.validate().is_err());
    }
    let source4 = configuration
        .route_source(Family::Ipv4, |_| Ok(Some(2)))
        .unwrap();
    let source6 = configuration
        .route_source(Family::Ipv6, |_| Ok(Some(2)))
        .unwrap();
    let plans = [plan(2, &addresses)];
    let intent = network_protocol::routing::Configuration::encode(
        &plans,
        [Some(&source4), Some(&source6)],
        None,
        None,
    )
    .unwrap();
    assert!(
        network_protocol::routing::Configuration::encode(
            &plans,
            [Some(&source6), Some(&source4)],
            None, None
        )
        .is_err()
    );
    for size in 0..intent.len() {
        assert!(network_protocol::routing::Configuration::parse(&intent[..size]).is_err());
    }
    let input = network_protocol::routing::Configuration::parse(&intent).unwrap();
    let mut routes = routing_rib::sources::Routes::default();
    let source_id = routes.reserve_source().unwrap();
    let changes = [
        routing_rib::sources::Routes::capture_source(
            source_id,
            routing_rib::sources::CONFIGURATION_DOMAIN,
            4,
            1,
            input.routes(Family::Ipv4).unwrap(),
        )
        .unwrap(),
        routing_rib::sources::Routes::capture_source(
            source_id,
            routing_rib::sources::CONFIGURATION_DOMAIN,
            4,
            1,
            input.routes(Family::Ipv6).unwrap(),
        )
        .unwrap(),
    ];
    let prepared = routes
        .prepare_configuration(&routing_facts(&net), &plans, &changes, None, None)
        .unwrap();
    let bytes = prepared.encoded;
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).unwrap();
    assert_eq!(
        batch.ipv6_forwarding().unwrap().next_hops().len(),
        1,
        "recursive prefixes share the resolved gateway group"
    );
    assert_eq!(batch.ipv6_forwarding().unwrap().iter().len(), 3);
    let transaction = crate::inet::prepare_configuration(
        &net,
        Some(network_protocol::ConfigurationBatch::parse(&bytes).unwrap()),
        None,
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let routes = prepared.next;
    advance_configuration(&net, 2002);
    frames(&device);
    let remote: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
    let frozen = route_snapshot::<Ipv6Addr>(&net).unwrap();
    let (four, six) = route_snapshots(&net).unwrap();
    assert_eq!(six.as_bytes(), frozen.as_bytes());
    assert_eq!(
        four.as_bytes(),
        route_snapshot::<Ipv4Addr>(&net).unwrap().as_bytes()
    );
    let native_route = frozen
        .iter()
        .find(|row| row.route.destination == "2001:db8:900::".parse::<Ipv6Addr>().unwrap())
        .unwrap();
    assert_eq!(native_route.single_path().unwrap().gateway, Some(PEER));
    assert!(frozen.iter().any(|row| row.route.destination == GLOBAL
        && row.route.kind == network_protocol::RouteKind::Local));
    assert!(frozen.iter().any(|row| row.route.destination
        == "2001:db8:dead::".parse::<Ipv6Addr>().unwrap()
        && row.route.kind == network_protocol::RouteKind::Blackhole));
    let peer = InetAddress::new(remote.into(), 44000);
    let endpoint = socket6(&net);
    endpoint.connect(peer).unwrap();
    endpoint.send_to(b"via-gateway", None, &[]).unwrap();
    poll(&net, 4004);
    poll(&net, 4004);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert!(matches!(
        NdPacket::parse(parse(&output[0]).1).unwrap().message(),
        NdMessage::NeighborSolicitation { target: PEER }
    ));
    enqueue(&device, request(PEER, LOCAL));
    poll(&net, 4005);
    let output = frames(&device);
    let frame = output
        .iter()
        .find(|frame| Ipv6Packet::parse(&frame[14..]).unwrap().upper_protocol() == 17)
        .unwrap();
    let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
    assert_eq!((ip.source(), ip.destination()), (GLOBAL, remote));
    assert_eq!(EthernetFrame::parse(frame).unwrap().destination(), MAC);
    assert_eq!(
        packet_wire::udp::UdpPacket::parse(ip.upper_layer().unwrap(), GLOBAL.into(), remote.into())
            .unwrap()
            .payload(),
        b"via-gateway"
    );
    enqueue(
        &device,
        datagram(remote, GLOBAL, endpoint.address().unwrap().port()),
    );
    poll(&net, 4006);
    assert_eq!(
        udp_read(&endpoint, false).unwrap(),
        (b"udp".to_vec(), Some(peer))
    );
    // 同一持久路由也必须用于本机控制回复，不能重新解析远端或要求它与本机直连。
    enqueue(&device, echo(remote, GLOBAL, b"routed-echo"));
    let unbound = datagram(remote, GLOBAL, 44001);
    enqueue(&device, unbound.clone());
    poll(&net, 4007);
    let output = frames(&device);
    assert_eq!(output.len(), 2);
    for frame in &output {
        let (ip, icmp) = parse(frame);
        assert_eq!((ip.source(), ip.destination()), (GLOBAL, remote));
        assert_eq!(EthernetFrame::parse(frame).unwrap().destination(), MAC);
        match (icmp.message_type(), icmp.code()) {
            (129, 0) => assert_eq!(&icmp.body()[4..], b"routed-echo"),
            (1, 4) => assert_eq!(&icmp.body()[4..], &unbound[14..]),
            other => panic!("unexpected routed reply {other:?}"),
        }
    }
    let mut changed = configuration.clone();
    changed.routes.push(StaticRoute {
        destination: "198.51.100.0".parse().unwrap(),
        prefix_length: 24,
        preference: 1,
        metric: 0,
        action: StaticAction::Discard,
    });
    let source4 = changed.route_source(Family::Ipv4, |_| Ok(Some(2))).unwrap();
    let update = routing_rib::sources::Routes::capture_source(
        source_id,
        routing_rib::sources::CONFIGURATION_DOMAIN,
        4,
        1,
        routing::Batch::parse(&source4).unwrap(),
    )
    .unwrap();
    let prepared = routes
        .prepare_sources(&routing_facts(&net), Some(update), &[])
        .unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
    assert_eq!(
        batch.ipv6_forwarding().unwrap().iter().len(),
        3,
        "IPv4 replacement cannot withdraw the same source's IPv6 declarations"
    );
    let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let routes = prepared.next;
    let empty = Configuration::default()
        .route_source(Family::Ipv6, |_| Ok(None))
        .unwrap();
    let clear = routing_rib::sources::Routes::capture_source(
        source_id,
        routing_rib::sources::CONFIGURATION_DOMAIN,
        4,
        1,
        routing::Batch::parse(&empty).unwrap(),
    )
    .unwrap();
    let prepared = routes
        .prepare_configuration(&routing_facts(&net), &[], &[clear], None, None)
        .unwrap();
    let transaction = crate::inet::prepare_configuration(
        &net,
        Some(network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap()),
        None,
    )
    .unwrap();
    transaction.activate().unwrap();
    assert!(
        !route_snapshot::<Ipv6Addr>(&net)
            .unwrap()
            .iter()
            .any(|row| row.route.protocol == 4)
    );
    assert!(
        route_snapshot::<Ipv4Addr>(&net)
            .unwrap()
            .iter()
            .any(|row| row.route.destination == "198.51.100.0".parse::<Ipv4Addr>().unwrap())
    );
    transaction.abort().unwrap();
    assert_eq!(
        route_snapshot::<Ipv6Addr>(&net)
            .unwrap()
            .iter()
            .filter(|row| row.route.protocol == 4)
            .count(),
        3
    );
    // 只改地址前缀时不撤回受管网关；热插入也不能丢失候选及逆向根中的路径字典。
    let changed = [definition(GLOBAL, 80, false)];
    let change = prepare_configuration(&net, [plan(2, &changed)].into_iter(), None).unwrap();
    change.activate().unwrap();
    let inserted = install_interface(
        &net,
        Arc::new(WireDevice::new()),
        crate::Channel,
        crate::Event::default(),
    )
    .unwrap();
    change.abort().unwrap();
    let network = stack(&net).unwrap().lock().unwrap();
    let selected = network
        .ipv6_routing
        .select(GLOBAL, remote, None, 0, &network.interfaces)
        .unwrap();
    assert_eq!((selected.interface, selected.neighbor), (2, PEER));
    assert!(network.interface(inserted).is_ok());
    drop(network);
    endpoint.close_description();
    use network_protocol::routes::{
        NextHopConfiguration, NextHopMember, RouteAttributes, RouteConfiguration,
    };
    configure_up(&net, inserted, true).unwrap();
    let members = [
        NextHopMember {
            interface_index: 2,
            gateway: Some(PEER),
            on_link: false,
            weight: 1,
        },
        NextHopMember {
            interface_index: inserted,
            gateway: Some(PEER),
            on_link: false,
            weight: 3,
        },
    ];
    let bytes = network_protocol::ConfigurationBatch::encode_forwarding(
        &[plan(2, &[]), plan(inserted, &addresses)],
        core::iter::empty(),
        core::iter::empty(),
        [NextHopConfiguration::new(9, &members)].into_iter(),
        [
            RouteConfiguration::Via {
                route: RouteAttributes {
                    preferred_source: None,
                    ..RouteAttributes::from(gateway_route(PEER))
                },
                next_hop: 9,
            },
            RouteConfiguration::Via {
                route: RouteAttributes {
                    destination: "2001:db8:500::".parse().unwrap(),
                    prefix_length: 64,
                    ..RouteAttributes::from(gateway_route(PEER))
                },
                next_hop: 9,
            },
        ]
        .into_iter(),
        &[],
        None,
    )
    .unwrap();
    let transaction = crate::inet::prepare_configuration(
        &net,
        Some(network_protocol::ConfigurationBatch::parse(&bytes).unwrap()),
        None,
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let before_removal = route_snapshot::<Ipv6Addr>(&net).unwrap();
    assert_eq!(
        before_removal
            .iter()
            .find(|row| row.route.prefix_length == 0)
            .unwrap()
            .next_hops
            .unwrap()
            .len(),
        2
    );
    assert!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .remove_interface(inserted)
    );
    let after_removal = route_snapshot::<Ipv6Addr>(&net).unwrap();
    assert!(
        after_removal
            .iter()
            .flat_map(|row| row.next_hops.into_iter().flat_map(|group| group.members()))
            .all(|member| member.interface_index != inserted)
    );
    assert!(
        !after_removal
            .iter()
            .any(|row| row.route.destination == "2001:db8:500::".parse::<Ipv6Addr>().unwrap()),
        "routes requiring a removed explicit source must retire even if another path remains"
    );
    let remaining = after_removal
        .iter()
        .find(|row| row.route.prefix_length == 0)
        .unwrap();
    assert_eq!(remaining.single_path().unwrap().interface_index, Some(2));
    assert_eq!(
        before_removal
            .iter()
            .find(|row| row.route.prefix_length == 0)
            .unwrap()
            .next_hops
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        native_route.single_path().unwrap().gateway,
        Some(PEER),
        "old snapshots retain their own route/group bytes"
    );
}
#[test]
fn gateway6_failed_neighbor_retires_all_remote_waiters_once_and_new_route_survives_old_failure() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for switch in [false, true] {
        let (net, device) = domain();
        ready(&net, &device);
        let addresses = [definition(GLOBAL, 64, true)];
        let transaction = prepare6(&net, &[plan(2, &addresses)], &[gateway_route(PEER)]).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        advance_configuration(&net, 2002);
        frames(&device);
        let remote: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
        let other: Ipv6Addr = "2001:db8:901::9".parse().unwrap();
        let endpoint = socket6(&net);
        endpoint
            .connect(InetAddress::new(remote.into(), 44000))
            .unwrap();
        for number in 0..8 {
            endpoint
                .send_to(
                    &[number],
                    (number % 2 != 0).then_some(InetAddress::new(other.into(), 44000)),
                    &[],
                )
                .unwrap();
        }
        for at in [4004, 4004, 5004, 6004] {
            poll(&net, at);
        }
        let replacement: Ipv6Addr = "fe80::3".parse().unwrap();
        if switch {
            let transaction = prepare6(&net, &[], &[gateway_route(replacement)]).unwrap();
            transaction.activate().unwrap();
            transaction.confirm().unwrap();
        }
        poll(&net, 7004);
        let output = frames(&device);
        let old_probes = output
            .iter()
            .filter(|frame| {
                let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
                ip.upper_protocol() == 58
                    && NdPacket::parse(parse(frame).1).is_ok_and(|nd| {
                        matches!(
                            nd.message(),
                            NdMessage::NeighborSolicitation { target: PEER }
                        )
                    })
            })
            .count();
        assert_eq!(old_probes, 3);
        let dropped = stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped;
        if switch {
            assert_eq!(
                dropped, 0,
                "old gateway failure must not discard new-route waiters"
            );
            assert_eq!(endpoint.take_error(), None);
            enqueue(&device, request(replacement, LOCAL));
            poll(&net, 7005);
            let output = frames(&device);
            let delivered = output
                .iter()
                .filter(|frame| Ipv6Packet::parse(&frame[14..]).unwrap().upper_protocol() == 17)
                .count();
            assert_eq!(delivered, 8);
        } else {
            assert_eq!(
                dropped, 8,
                "all destinations using the failed gateway must retire"
            );
            assert_eq!(endpoint.take_error(), Some(SocketError::HostUnreachable));
            assert_eq!(endpoint.take_error(), None);
        }
        assert!(endpoint.poll_state().writable);
        endpoint.close_description();
    }
}
#[test]
fn gateway6_admission_and_mixed_family_rollback_never_publish_partial_configuration() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, true)];
    let original = stack(&net)
        .unwrap()
        .lock()
        .unwrap()
        .interface(2)
        .unwrap()
        .state;
    let mut plan = plan(2, &addresses);
    plan.address = Some(Ipv4Addr::new(192, 0, 2, 1));
    plan.prefix_length = 24;
    for route in [
        gateway_route("2001:db8:999::1".parse().unwrap()),
        network_protocol::RouteRequest {
            preferred_source: Some(Ipv6Addr::LOCALHOST),
            ..gateway_route(PEER)
        },
    ] {
        assert!(prepare6(&net, &[plan], &[route]).is_err());
        assert!(
            stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .interface(2)
                .unwrap()
                .state
                == original
        );
        assert!(row(&net, GLOBAL).is_none());
    }
    // 每个实际预备分配点失败都应释放配置租约，不能让另一地址族提前生效。
    let destination4 = Ipv4Addr::new(198, 51, 100, 0);
    let route4 = network_protocol::RouteRequest {
        destination: destination4,
        prefix_length: 24,
        table: 254,
        metric: 1,
        scope: 253,
        protocol: 3,
        kind: network_protocol::RouteKind::Unicast,
        interface_index: Some(2),
        gateway: None,
        preferred_source: None,
        metrics: network_protocol::RouteMetrics::default(),
        on_link: false,
    };
    let bytes = network_protocol::ConfigurationBatch::encode_forwarding(
        &[plan],
        core::iter::empty(),
        [network_protocol::routes::RouteConfiguration::Inline(route4)].into_iter(),
        core::iter::empty(),
        [network_protocol::routes::RouteConfiguration::Inline(
            gateway_route(PEER),
        )]
        .into_iter(),
        &[],
        None,
    )
    .unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).unwrap();
    let mut prepared = None;
    for fault in 0..128 {
        crate::allocation_failure::fail_after(fault);
        let result = crate::inet::prepare_configuration(&net, Some(batch), None);
        let consumed = crate::allocation_failure::was_consumed();
        match result {
            Ok(transaction) => {
                assert!(!consumed);
                prepared = Some(transaction);
                break;
            }
            Err(error) => {
                assert!(consumed);
                assert_eq!(error, SocketError::NoMemory);
            }
        }
        assert!(
            stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .interface(2)
                .unwrap()
                .state
                == original
        );
        assert!(row(&net, GLOBAL).is_none());
        assert!(
            stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .route(Ipv4Addr::UNSPECIFIED, destination4, None)
                .is_err()
        );
    }
    let transaction = prepared.expect("bounded actual preparation allocations");
    crate::allocation_failure::fail_next();
    transaction.activate().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    poll(&net, 2002);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Tentative);
    let remote: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
    {
        let network = stack(&net).unwrap().lock().unwrap();
        assert_eq!(
            network
                .route(Ipv4Addr::UNSPECIFIED, destination4, None)
                .unwrap()
                .interface_index,
            2
        );
        assert_eq!(
            network
                .ipv6_routing
                .select(Ipv6Addr::UNSPECIFIED, remote, None, 0, &network.interfaces)
                .unwrap()
                .neighbor,
            PEER
        );
    }
    crate::allocation_failure::fail_next();
    transaction.abort().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    assert!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .state
            == original
    );
    assert!(row(&net, GLOBAL).is_none());
    let network = stack(&net).unwrap().lock().unwrap();
    assert!(
        network
            .route(Ipv4Addr::UNSPECIFIED, destination4, None)
            .is_err()
    );
    assert!(
        network
            .ipv6_routing
            .select(Ipv6Addr::UNSPECIFIED, remote, None, 0, &network.interfaces)
            .is_err()
    );
}

// 单 NIC 网关回归无法证明非对称路径和入接口组播源地址；使用同一 NET 中的两个实际 NIC。
#[test]
fn ipv6_control_replies_choose_the_fib_output_and_the_ingress_multicast_source() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, ingress) = domain();
    ready(&net, &ingress);
    let egress = Arc::new(WireDevice::new());
    let index = install_interface(
        &net,
        egress.clone(),
        crate::Channel,
        crate::Event::default(),
    )
    .unwrap();
    let other: Ipv6Addr = "2001:db8:2::1".parse().unwrap();
    let incoming_addresses = [definition(GLOBAL, 64, true)];
    let outgoing_addresses = [definition(other, 64, true)];
    let mut route = gateway_route(PEER);
    route.interface_index = Some(index);
    route.preferred_source = None;
    let transaction = prepare6(
        &net,
        &[
            plan(2, &incoming_addresses),
            plan(index, &outgoing_addresses),
        ],
        &[route],
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    advance_configuration(&net, 2002);
    frames(&ingress);
    frames(&egress);
    let remote: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
    enqueue(&ingress, echo(remote, GLOBAL, b"asymmetric"));
    let unbound = datagram(remote, GLOBAL, 44001);
    enqueue(&ingress, unbound.clone());
    poll(&net, 4004);
    assert!(frames(&ingress).is_empty());
    let pending = frames(&egress);
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        NdPacket::parse(parse(&pending[0]).1).unwrap().message(),
        NdMessage::NeighborSolicitation { target: PEER }
    ));
    enqueue(&egress, request(PEER, other));
    poll(&net, 4005);
    assert!(frames(&ingress).is_empty());
    let output = frames(&egress);
    let replies = output
        .iter()
        .filter(|frame| parse(frame).0.destination() == remote)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 2);
    for frame in replies {
        let (ip, icmp) = parse(frame);
        assert_eq!((ip.source(), ip.destination()), (GLOBAL, remote));
        assert_eq!(EthernetFrame::parse(frame).unwrap().destination(), MAC);
        match (icmp.message_type(), icmp.code()) {
            (129, 0) => assert_eq!(&icmp.body()[4..], b"asymmetric"),
            (1, 4) => assert_eq!(&icmp.body()[4..], &unbound[14..]),
            other => panic!("unexpected asymmetric reply {other:?}"),
        }
    }
    enqueue(&ingress, echo(remote, ALL, b"multicast-ingress-source"));
    poll(&net, 4006);
    assert!(frames(&ingress).is_empty());
    let output = frames(&egress);
    assert_eq!(output.len(), 1);
    let (ip, icmp) = parse(&output[0]);
    assert_eq!((ip.source(), ip.destination()), (GLOBAL, remote));
    assert_eq!(&icmp.body()[4..], b"multicast-ingress-source");
    // 链路范围没有跨 zone 的默认路由回退；不凭同名链路地址把回复发往另一个 NIC。
    configure_up(&net, 2, false).unwrap();
    enqueue(&egress, echo(PEER, LOCAL, b"egress-link"));
    poll(&net, 4007);
    assert!(frames(&ingress).is_empty());
    let output = frames(&egress);
    assert!(
        output
            .iter()
            .any(|frame| parse(frame).1.message_type() == 129)
    );
}

// The datagram is queued before NUD fails, when its first eligible interface is still preferred.
#[test]
fn queued_ipv6_datagram_switches_interfaces_and_recovers_the_preferred_router() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, first) = domain();
    ready(&net, &first);
    let second = Arc::new(WireDevice::new());
    let index = install_interface(
        &net,
        second.clone(),
        crate::Channel,
        crate::Event::default(),
    )
    .unwrap();
    let other = "2001:db8:2::1".parse().unwrap();
    let transaction = prepare_configuration(
        &net,
        [
            plan(2, &[definition(GLOBAL, 64, true)]),
            plan(index, &[definition(other, 64, true)]),
        ]
        .into_iter(),
        Some(network_protocol::Forwarding {
            ipv4: true,
            ipv6: false,
        }),
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    advance_configuration(&net, 2002);
    frames(&first);
    frames(&second);

    let advertise = |device: &WireDevice, mac: &[u8; 6]| {
        let mut frame = vec![0; 78];
        OptionValue::LinkLayerAddress {
            source: true,
            address: mac,
        }
        .emit(&mut frame[70..])
        .unwrap();
        NdMessage::RouterAdvertisement {
            hop_limit: 64,
            flags: 0,
            lifetime: 120,
            reachable_time: 0,
            retransmit_timer: 0,
        }
        .emit(&mut frame[54..], PEER, ALL)
        .unwrap();
        envelope(&mut frame, PEER, ALL, false, 255);
        enqueue(device, frame);
    };
    let second_mac = [2, 0, 0, 0, 0, 33];
    // The identical link-local address represents two independent next hops.
    advertise(&first, &MAC);
    advertise(&second, &second_mac);
    poll(&net, 5000);
    frames(&first);
    frames(&second);

    let destination = "2001:db8:ff::2".parse::<Ipv6Addr>().unwrap();
    let socket = socket6(&net);
    let enqueue_payload = |bytes: &[u8]| {
        socket
            .send_to(
                bytes,
                Some(InetAddress::new(destination.into(), 44333)),
                &[],
            )
            .unwrap();
    };
    let data = |frames: &[Vec<u8>]| {
        frames
            .iter()
            .filter_map(|frame| {
                let ethernet = EthernetFrame::parse(frame).ok()?;
                let ip = Ipv6Packet::parse(ethernet.payload()).ok()?;
                (ip.upper_protocol() == 17 && ip.destination() == destination)
                    .then_some((ethernet.destination(), ip.source()))
            })
            .collect::<Vec<_>>()
    };
    enqueue_payload(b"preferred");
    poll(&net, 5001);
    assert_eq!(data(&frames(&first)), [(MAC, GLOBAL)]);
    assert!(data(&frames(&second)).is_empty());
    for time in [10001, 11001, 12001] {
        poll(&net, time);
    }
    frames(&first);
    frames(&second);

    enqueue_payload(b"already queued before failure");
    poll(&net, 13001);
    assert!(data(&frames(&first)).is_empty());
    assert_eq!(data(&frames(&second)), [(second_mac, GLOBAL)]);
    poll(&net, 13002);
    assert!(frames(&first).iter().any(|frame| {
        let (_, icmp) = parse(frame);
        NdPacket::parse(icmp)
            .is_ok_and(|nd| nd.message() == NdMessage::NeighborSolicitation { target: PEER })
    }));

    let mut reply = vec![0; 78];
    OptionValue::LinkLayerAddress {
        source: false,
        address: &MAC,
    }
    .emit(&mut reply[78 - 8..])
    .unwrap();
    NdMessage::NeighborAdvertisement {
        flags: 0xe000_0000,
        target: PEER,
    }
    .emit(&mut reply[54..], PEER, LOCAL)
    .unwrap();
    envelope(&mut reply, PEER, LOCAL, false, 255);
    enqueue(&first, reply);
    poll(&net, 13003);
    frames(&first);
    enqueue_payload(b"recovered");
    poll(&net, 13004);
    assert_eq!(data(&frames(&first)), [(MAC, GLOBAL)]);
    assert!(data(&frames(&second)).is_empty());
    socket.close_description();
}

// Exercise the published static route, a real NUD failure, and subsequent traffic on the backup.
