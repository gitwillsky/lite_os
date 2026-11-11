use router_protocol::configuration::Configuration;
use router_protocol::routing::{Batch, Family};
use routing_rib::sources::{CONFIGURATION_DOMAIN, Routes, STATIC_PROTOCOL};

// 经生产配置、RIB 和真实 NIC 验证：IPv6 ND 解析出口，转发仍提交原生 IPv4 报文。
#[test]
fn ipv4_static_route_uses_an_ipv6_neighbor_on_an_ipv6_only_egress() {
    use packet_wire::ipv4::{Ipv4Header, Ipv4Packet};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, ingress, egress) = routing_domain(89252);
    configure_address(&net, 3, Ipv4Addr::UNSPECIFIED).unwrap();
    ready(&net, &egress);
    let config = Configuration::decode(br#"{
      "version":1,
      "interfaces":{"uplink":{"device":{"kind":"name","value":"eth1"}}},
      "routes":[{"destination":"203.0.113.0","prefix_length":24,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"uplink","gateway":"fe80::2"}}]}}]
    }"#).unwrap();
    let bytes = config
        .route_source(Family::Ipv4, |name| {
            Ok(name.matches(b"eth1", [0; 6]).then_some(3))
        })
        .unwrap();
    let mut routes = Routes::default();
    let source = Routes::capture_source(
        routes.reserve_source().unwrap(),
        CONFIGURATION_DOMAIN,
        STATIC_PROTOCOL,
        1,
        Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    let candidate = routes
        .prepare_sources(&routing_facts(&net), Some(source), &[])
        .unwrap();
    let transaction = crate::inet::prepare_configuration(
        &net,
        Some(network_protocol::ConfigurationBatch::parse(&candidate.encoded).unwrap()),
        None,
    )
    .unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let destination = Ipv4Addr::new(203, 0, 113, 9);
    let lookup = network_protocol::routes::RouteLookup {
        destination: destination.into(),
        source: Ipv4Addr::UNSPECIFIED.into(),
        direction: network_protocol::routes::RouteDirection::Output(None),
        mark: 0,
    };
    let path = crate::inet::configuration::route_lookup(&net, lookup).unwrap();
    assert_eq!(
        (path.interface_index, path.source, path.gateway, path.table),
        (3, Ipv4Addr::new(10, 40, 0, 15).into(), Some(PEER.into()), 254)
    );
    assert_eq!(
        crate::inet::configuration::route_lookup(&net, network_protocol::routes::RouteLookup {
            source: Ipv4Addr::new(203, 0, 113, 1).into(), ..lookup
        }),
        Err(SocketError::AddressNotAvailable)
    );
    let snapshot = route_snapshot::<Ipv4Addr>(&net).unwrap();
    assert!(
        snapshot
            .iter()
            .filter_map(|route| route.single_path())
            .any(|route| {
                route.destination == Ipv4Addr::new(203, 0, 113, 0)
                    && route.interface_index == Some(3)
                    && route.gateway == Some(PEER.into())
            })
    );
    let payload = b"IPv4 transit through an IPv6 neighbor";
    let mut frame = vec![0; 34 + payload.len()];
    EthernetHeader {
        source: MAC,
        destination: [2, 0, 0, 0, 0, 1],
        type_or_length: 0x0800,
        tags: &[],
    }
    .emit(&mut frame)
    .unwrap();
    let source = Ipv4Addr::new(10, 40, 0, 9);
    frame[34..].copy_from_slice(payload);
    Ipv4Header {
        source,
        destination,
        protocol: 253,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 101,
        fragment: 0,
        options: &[],
    }
    .emit(&mut frame[14..])
    .unwrap();
    enqueue(&ingress, frame);
    poll(&net, 3000);
    poll(&net, 3001);
    assert!(sent_gateway_probe(&egress, PEER));
    reply_gateway(&egress, PEER, &MAC);
    poll(&net, 3002);
    let output = frames(&egress);
    let wire = output
        .iter()
        .filter_map(|frame| EthernetFrame::parse(frame).ok())
        .find(|frame| frame.type_or_length() == 0x0800)
        .unwrap();
    assert_eq!(wire.destination(), MAC);
    let ip = Ipv4Packet::parse(wire.payload()).unwrap();
    assert_eq!(
        (ip.source(), ip.destination(), ip.hop_limit(), ip.payload()),
        (source, destination, 63, payload.as_slice())
    );

    let socket = scoped_endpoint(&net, InetProtocol::Udp);
    let remote = InetAddress::new(destination.into(), 40400);
    socket.send_to(b"local", Some(remote), &[]).unwrap();
    poll(&net, 3003);
    let output = frames(&egress);
    let wire = output
        .iter()
        .filter_map(|frame| EthernetFrame::parse(frame).ok())
        .find(|frame| frame.type_or_length() == 0x0800)
        .unwrap();
    let ip = Ipv4Packet::parse(wire.payload()).unwrap();
    assert_eq!(wire.destination(), MAC);
    assert_eq!(
        (ip.source(), ip.destination(), ip.protocol()),
        (Ipv4Addr::new(10, 40, 0, 15), destination, 17)
    );
    assert_eq!(&ip.payload()[8..], b"local");
    socket.close_description();
}

// 直连跨族测试不覆盖两族同次编译、上游撤回及反事实可达性；从同一配置声明验证两族的路由事务。
#[test]
fn recursive_ipv4_route_follows_ipv6_underlay_preference_and_withdrawal() {
    use packet_wire::ipv4::Ipv4Packet;
    use router_protocol::tracking::{Query, Snapshot, State as RouteState, installation};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, backup, primary) = routing_domain(89253);
    ready(&net, &primary);
    let configuration = Configuration::decode(br#"{
      "version":1,
      "interfaces":{
        "backup":{"device":{"kind":"name","value":"eth0"}},
        "primary":{"device":{"kind":"name","value":"eth1"}}
      },
      "routes":[
        {"destination":"203.0.113.9","prefix_length":32,"action":{"kind":"forward","members":[{"target":{"kind":"recursive","address":"2001:db8:900::9"}}]}},
        {"destination":"2001:db8:900::9","prefix_length":128,"preference":10,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"primary","gateway":"fe80::2"}}]}},
        {"destination":"2001:db8:900::9","prefix_length":128,"preference":200,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"backup","gateway":"fe80::3"}}]}}
      ]
    }"#).unwrap();
    let mut routes = Routes::default();
    let mut capture = |family| {
        let bytes = configuration.route_source(family, |device| {
            Ok([(2, b"eth0"), (3, b"eth1")].into_iter()
                .find_map(|(index, name)| device.matches(name, [0; 6]).then_some(index)))
        }).unwrap();
        let id = routes.reserve_source().unwrap();
        let captured = Routes::capture_source(id, CONFIGURATION_DOMAIN,
            STATIC_PROTOCOL, 1, Batch::parse(&bytes).unwrap()).unwrap();
        (id, captured)
    };
    let (source, source4) = capture(Family::Ipv4);
    let (underlay, source6) = capture(Family::Ipv6);
    let activate = |prepared: routing_rib::sources::PreparedRoutes| {
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap();
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        prepared.next
    };
    let initial = routes.prepare_configuration(&routing_facts(&net), &[], &[source4, source6], None, None).unwrap();
    routes = activate(initial);
    let destination = Ipv4Addr::new(203, 0, 113, 9);
    let target = Query { address: destination.into(), prefix_length: None, resolve_via_default: false };
    let logical = Query { address: "2001:db8:900::9".parse().unwrap(), ..target };
    let candidate = "192.0.2.1".parse().unwrap();
    let probes = [
        installation::Installation { interface: None, network: candidate, length: 32, next_hop: logical },
        installation::Installation { interface: None, network: candidate, length: 32, next_hop: target },
        installation::Installation { interface: None, network: destination.into(), length: 32, next_hop: target },
    ];
    let observe = |routes: &Routes, gateway: Option<router_protocol::tracking::Adjacency>| {
        let snapshot = routes.snapshot().unwrap();
        let encoded = snapshot.encode(&[target]).unwrap();
        let view = Snapshot::parse(&encoded).unwrap().records().next().unwrap();
        assert_eq!(view.state, if gateway.is_some() { RouteState::Forward } else { RouteState::NoRoute });
        assert_eq!(view.members().next().map(|member| member.target), gateway);
        let bytes = snapshot.evaluate_installations(source, &probes).unwrap();
        let verdicts = installation::decode_outcomes(&bytes, source, snapshot.generation(), probes.len()).unwrap();
        assert!(verdicts.iter().map(|verdict| verdict.reachable).eq([gateway.is_some(), gateway.is_some(), false]));
    };
    observe(&routes, Some(router_protocol::tracking::Adjacency {
        interface_index: 3, gateway: Some(PEER.into()), on_link: false,
    }));
    let endpoint = scoped_endpoint(&net, InetProtocol::Udp);
    let remote = InetAddress::new(destination.into(), 41000);
    let send = |egress: &WireDevice, next: Ipv6Addr, time| {
        enqueue(egress, request(next, LOCAL));
        poll(&net, time);
        frames(egress);
        endpoint.send_to(b"recursive", Some(remote), &[]).unwrap();
        poll(&net, time + 1);
        assert!(frames(egress).iter().any(|frame| {
            let wire = EthernetFrame::parse(frame).unwrap();
            wire.type_or_length() == 0x0800 && Ipv4Packet::parse(wire.payload()).is_ok_and(|ip| {
                ip.destination() == destination && ip.protocol() == 17 && wire.destination() == MAC
            })
        }));
    };
    send(&primary, PEER, 3000);

    configure_up(&net, 3, false).unwrap();
    routes = activate(routes.prepare_sources(&routing_facts(&net), None, &[]).unwrap());
    let alternate: Ipv6Addr = "fe80::3".parse().unwrap();
    observe(&routes, Some(router_protocol::tracking::Adjacency {
        interface_index: 2, gateway: Some(alternate.into()), on_link: false,
    }));
    send(&backup, alternate, 4000);

    routes = activate(routes.prepare_sources(&routing_facts(&net), None, &[underlay]).unwrap());
    observe(&routes, None);
    assert_eq!(endpoint.send_to(b"withdrawn", Some(remote), &[]), Err(SocketError::NetworkUnreachable));
    endpoint.close_description();
}

fn install_static_gateway_routes(net: &Arc<crate::namespace::Namespace>) -> Routes {
    let configuration = Configuration::decode(br#"{
      "version":1,
      "interfaces":{"uplink":{"device":{"kind":"name","value":"eth0"}}},
      "routes":[
        {"destination":"2001:db8:900::9","prefix_length":128,"preference":200,"metric":1,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"uplink","gateway":"fe80::3"}}]}},
        {"destination":"2001:db8:900::9","prefix_length":128,"preference":10,"metric":100,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"uplink","gateway":"fe80::2"}}]}},
        {"destination":"2001:db8:900::9","prefix_length":128,"preference":1,"action":{"kind":"forward","members":[{"target":{"kind":"recursive","address":"2001:db8:dead::9"}}]}},
        {"destination":"2001:db8:901::9","prefix_length":128,"action":{"kind":"forward","members":[{"target":{"kind":"recursive","address":"2001:db8:900::9"}}]}}
      ]
    }"#).unwrap();
    let bytes = configuration
        .route_source(Family::Ipv6, |_| Ok(Some(2)))
        .unwrap();
    let mut routes = Routes::default();
    let id = routes.reserve_source().unwrap();
    let source = Routes::capture_source(
        id,
        CONFIGURATION_DOMAIN,
        STATIC_PROTOCOL,
        1,
        Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    let addresses = [definition(GLOBAL, 64, true)];
    let plans = [plan(2, &addresses)];
    let prepared = routes
        .prepare_configuration(&routing_facts(net), &plans, &[source], None, None)
        .unwrap();
    let configuration = crate::inet::prepare_configuration(
        net,
        Some(network_protocol::ConfigurationBatch::parse(&prepared.encoded).unwrap()),
        None,
    )
    .unwrap();
    configuration.activate().unwrap();
    configuration.confirm().unwrap();
    advance_configuration(net, 2002);
    prepared.next
}

fn sent_gateways(device: &WireDevice, destination: Ipv6Addr) -> Vec<[u8; 6]> {
    frames(device)
        .iter()
        .filter_map(|frame| {
            let ethernet = EthernetFrame::parse(frame).ok()?;
            let ip = Ipv6Packet::parse(ethernet.payload()).ok()?;
            (ip.upper_protocol() == 17 && ip.destination() == destination)
                .then_some(ethernet.destination())
        })
        .collect::<Vec<_>>()
}

fn sent_gateway_probe(device: &WireDevice, target: Ipv6Addr) -> bool {
    frames(device).iter().any(|frame| {
        let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
        if ip.upper_protocol() != 58 {
            return false;
        }
        let (_, icmp) = parse(frame);
        NdPacket::parse(icmp)
            .is_ok_and(|nd| nd.message() == NdMessage::NeighborSolicitation { target })
    })
}

// 静态来源保留失败候选，协议可撤空其 FIB；现有静态复探用例无法覆盖这种寿命差异。
#[test]
fn withdrawn_bgp_route_can_recover_its_failed_gateway_without_traffic() {
    use router_protocol::tracking::{Query, State as RouteState, installation::Installation};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let destination: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
    let configuration = Configuration::decode(br#"{
      "version":1,
      "interfaces":{"uplink":{"device":{"kind":"name","value":"eth0"}}},
      "routes":[{"destination":"2001:db8:900::9","prefix_length":128,"preference":20,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"uplink","gateway":"fe80::2","on_link":true}}]}}]
    }"#).unwrap();
    let bytes = configuration.route_source(Family::Ipv6, |_| Ok(Some(2))).unwrap();
    let empty = Batch::encode(
        Family::Ipv6,
        &[],
        [] as [router_protocol::routing::NextHop<'_>; 0],
        [] as [router_protocol::routing::Route; 0],
    )
    .unwrap();
    let mut routes = Routes::default();
    let source = routes.reserve_source().unwrap();
    let capture = |bytes: &[u8]| {
        Routes::capture_source(
            source,
            source,
            router_protocol::bgp::ROUTE_PROTOCOL,
            20,
            Batch::parse(bytes).unwrap(),
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
    routes =
        install(routes.prepare_sources(&routing_facts(&net), Some(capture(&bytes)), &[]).unwrap());
    let socket = socket6(&net);
    let peer = InetAddress::new(destination.into(), 44000);
    enqueue(&device, request(PEER, LOCAL));
    poll(&net, 4000);
    frames(&device);
    socket.send_to(b"before failure", Some(peer), &[]).unwrap();
    poll(&net, 4001);
    assert_eq!(sent_gateways(&device, destination), [MAC]);
    for time in [9001, 10001, 11001, 12001] {
        poll(&net, time);
    }
    frames(&device);
    routes = install(routes.prepare_sources(&routing_facts(&net), None, &[]).unwrap());

    let destination_query =
        Query { address: destination.into(), prefix_length: None, resolve_via_default: false };
    let probe = Installation {
        network: destination.into(),
        length: 128,
        interface: core::num::NonZeroU32::new(2),
        next_hop: Query { address: PEER.into(), ..destination_query },
    };
    let removed = routes.prepare_source(&routing_facts(&net), capture(&empty), &[probe]).unwrap();
    assert!(!removed.outcomes[0].reachable);
    assert!(removed.recovery_changed);
    assert_eq!(
        removed.next.snapshot().unwrap().lookup(destination_query).state,
        RouteState::NoRoute
    );
    routes = install(removed);

    let facts = routing_facts(&net);
    let unchanged = routes.prepare_source(&facts, capture(&empty), &[probe]).unwrap();
    assert!(!unchanged.recovery_changed);
    let retired = routes.prepare_sources(&facts, None, &[source]).unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&retired.encoded).unwrap();
    assert_eq!(batch.gateway_probes().len(), 0);
    let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    transaction.activate().unwrap();
    transaction.abort().unwrap();
    routes = install(routes.prepare_sources(&routing_facts(&net), None, &[]).unwrap());

    // 不触发应用发送，也不由邻居主动发通告。
    poll(&net, 12002);
    assert!(
        sent_gateway_probe(&device, PEER),
        "the withdrawn protocol route still needs gateway recovery"
    );
    reply_gateway(&device, PEER, &MAC);
    poll(&net, 12003);
    let recovered = routes.prepare_source(&routing_facts(&net), capture(&bytes), &[probe]).unwrap();
    assert!(recovered.outcomes[0].reachable);
    assert!(recovered.recovery_changed);
    let batch = network_protocol::ConfigurationBatch::parse(&recovered.encoded).unwrap();
    assert_eq!(batch.gateway_probes().len(), 0);
    install(recovered);
    socket.send_to(b"after recovery", Some(peer), &[]).unwrap();
    poll(&net, 12004);
    assert_eq!(sent_gateways(&device, destination), [MAC]);
    socket.close_description();
}

fn reply_gateway(device: &WireDevice, address: Ipv6Addr, mac: &[u8; 6]) {
    let mut frame = vec![0; 86];
    OptionValue::LinkLayerAddress {
        source: false,
        address: mac,
    }
    .emit(&mut frame[78..])
    .unwrap();
    NdMessage::NeighborAdvertisement {
        flags: 0x6000_0000,
        target: address,
    }
    .emit(&mut frame[54..], address, LOCAL)
    .unwrap();
    envelope(&mut frame, address, LOCAL, false, 255);
    frame[6..12].copy_from_slice(mac);
    enqueue(device, frame);
}

#[test]
fn static_ipv6_gateway_uses_backup_until_neighbor_confirms_recovery() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let alternate: Ipv6Addr = "fe80::3".parse().unwrap();
    let destination: Ipv6Addr = "2001:db8:900::9".parse().unwrap();
    let alternate_mac = [2, 0, 0, 0, 0, 33];
    install_static_gateway_routes(&net);
    let lookup = |destination| {
        crate::inet::configuration::route_lookup(&net, network_protocol::routes::RouteLookup {
            destination: core::net::IpAddr::V6(destination),
            source: Ipv6Addr::UNSPECIFIED.into(),
            direction: network_protocol::routes::RouteDirection::Output(None),
        mark: 0,
        }).unwrap()
    };
    let local = lookup(Ipv6Addr::LOCALHOST);
    assert_eq!((local.interface_index, local.table, local.kind, local.gateway),
        (1, 255, network_protocol::RouteKind::Local, None));
    assert_eq!(lookup(destination).gateway, Some(PEER.into()));
    let snapshot = route_snapshot::<Ipv6Addr>(&net).unwrap();
    let paths = snapshot.iter().filter(|row| row.route.destination == destination);
    assert!(paths.map(|row| row.route.metric).eq([100, 1]));

    enqueue(&device, request(PEER, LOCAL));
    poll(&net, 4000);
    frames(&device);

    let socket = socket6(&net);
    let peer = InetAddress::new(destination.into(), 44333);
    let queue = || socket.send_to(b"static route", Some(peer), &[]).unwrap();
    let data = || sent_gateways(&device, destination);
    queue();
    poll(&net, 4001);
    assert_eq!(data(), [MAC]);
    for time in [9001, 10001, 11001] {
        poll(&net, time);
    }
    frames(&device);

    queue();
    poll(&net, 12001);
    let snapshot = crate::inet::configuration::neighbor_snapshot(&net, 2).unwrap();
    let gateway = (0..snapshot.len())
        .filter_map(|index| snapshot.get(index))
        .find(|row| row.address == PEER)
        .unwrap();
    assert_eq!(gateway.state, network_protocol::NeighborState::Failed);
    assert!(!gateway.is_router, "static configuration leaves the Router flag intact");
    assert!(data().is_empty());

    poll(&net, 12002);
    assert!(sent_gateway_probe(&device, alternate));
    reply_gateway(&device, alternate, &alternate_mac);
    poll(&net, 12003);
    assert_eq!(data(), [alternate_mac]);
    assert_eq!(lookup(destination).gateway, Some(alternate.into()));

    poll(&net, 12004);
    assert!(sent_gateway_probe(&device, PEER));
    reply_gateway(&device, PEER, &MAC);
    poll(&net, 12005);
    queue();
    poll(&net, 12006);
    assert_eq!(data(), [MAC]);
    assert_eq!(lookup(destination).gateway, Some(PEER.into()));
    socket.close_description();
}

// 直接发往主备前缀的流量会触发现有按需复探，无法验证递归路径被撤下后仍能自动恢复。
#[test]
fn recursive_ipv6_route_switches_and_recovers_without_traffic_to_parent_prefix() {
    use core::sync::atomic::Ordering;
    use network_protocol::{ConfigurationBatch, NetworkInstaller};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let changes = Arc::new(crate::Event::default());
    observe_interfaces(&net, changes.clone()).unwrap();
    let device = Arc::new(WireDevice::new());
    let index = install_interface(&net, device.clone(), crate::Channel, crate::Event::default()).unwrap();
    configure_up(&net, index, true).unwrap();
    ready(&net, &device);
    let mut routes = install_static_gateway_routes(&net);
    let alternate: Ipv6Addr = "fe80::3".parse().unwrap();
    let alternate_mac = [2, 0, 0, 0, 0, 33];
    let destination: Ipv6Addr = "2001:db8:901::9".parse().unwrap();
    let endpoint = socket6(&net);
    let peer = InetAddress::new(destination.into(), 44333);
    let send = || endpoint.send_to(b"recursive route", Some(peer), &[]).unwrap();
    let prepare = |bytes| {
        crate::inet::prepare_configuration(&net, Some(ConfigurationBatch::parse(bytes).unwrap()), None)
            .unwrap()
    };
    let input_mask = NetworkInstaller::TOPOLOGY_CHANGED | NetworkInstaller::GATEWAYS_CHANGED;
    changes.signal(input_mask, 0).unwrap();
    enqueue(&device, request(PEER, LOCAL));
    poll(&net, 4000);
    frames(&device);
    send();
    poll(&net, 4001);
    assert_eq!(sent_gateways(&device, destination), [MAC]);
    for time in [9001, 10001, 11001, 12001] {
        poll(&net, time);
    }
    frames(&device);
    assert_eq!(changes.0.load(Ordering::Relaxed), NetworkInstaller::GATEWAYS_CHANGED);
    changes.signal(input_mask, 0).unwrap();

    let backup = routes.prepare_sources(&routing_facts(&net), None, &[]).unwrap();
    let batch = ConfigurationBatch::parse(&backup.encoded).unwrap();
    assert!(batch.gateway_probes().eq([(index, PEER.into())]));
    let transaction = prepare(&backup.encoded);
    transaction.activate().unwrap();
    transaction.abort().unwrap();
    poll(&net, 12002);
    assert!(!sent_gateway_probe(&device, PEER), "rollback retires the new probe");
    let transaction = prepare(&backup.encoded);
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    routes = backup.next;
    poll(&net, 12003);
    assert!(sent_gateway_probe(&device, PEER), "the failed gateway stays monitored after its route retires");

    send();
    for time in [12004, 12005] {
        poll(&net, time);
    }
    assert!(sent_gateway_probe(&device, alternate));
    reply_gateway(&device, alternate, &alternate_mac);
    poll(&net, 12006);
    assert_eq!(sent_gateways(&device, destination), [alternate_mac]);
    poll(&net, 72002);
    assert!(!sent_gateway_probe(&device, PEER));
    poll(&net, 72003);
    assert!(sent_gateway_probe(&device, PEER), "monitor retries without new application traffic");
    assert_eq!(changes.0.load(Ordering::Relaxed), 0);
    reply_gateway(&device, PEER, &MAC);
    poll(&net, 72004);
    assert_eq!(changes.0.load(Ordering::Relaxed), NetworkInstaller::GATEWAYS_CHANGED);
    let recovered = routes.prepare_sources(&routing_facts(&net), None, &[]).unwrap();
    assert_eq!(ConfigurationBatch::parse(&recovered.encoded).unwrap().gateway_probes().len(), 0);
    let transaction = prepare(&recovered.encoded);
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    send();
    poll(&net, 72005);
    assert_eq!(sent_gateways(&device, destination), [MAC]);
    endpoint.close_description();
}
