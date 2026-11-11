// 上层重试无法证明 ARP 只有在真实提交后才计数，直接检查 NIC 与同一邻居 owner。
#[test]
fn native_arp_gateway_exhausts_committed_probes_and_recovers_from_valid_reply() {
    use core::sync::atomic::Ordering;
    use packet_wire::{arp::ArpPacket, ethernet::EthernetFrame};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, device) = routing_domain(89245);
    let local = Ipv4Addr::new(10, 41, 0, 15);
    let gateway = Ipv4Addr::new(10, 41, 0, 1);
    let destination = Ipv4Addr::new(203, 0, 113, 9);
    let route = network_protocol::Ipv4RouteRequest {
        gateway: Some(gateway.into()),
        scope: 0,
        ..static_route(destination, 32, 3, 7)
    };
    configure_route(
        &net,
        route,
        network_protocol::RouteChange {
            delete: false,
            replace: false,
            create: true,
            exclusive: false,
            append: false,
        },
    )
    .unwrap();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender
        .send_to(
            b"arp-gateway",
            Some(InetAddress::new(destination.into(), 44331)),
            &[],
        )
        .unwrap();
    let poll = |at| {
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(at))
    };
    let rank = || {
        stack(&net).unwrap().lock().unwrap().interfaces[2]
            .arp
            .router_rank(gateway)
    };
    let count = || {
        device
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|frame| {
                let ethernet = EthernetFrame::parse(frame).unwrap();
                ethernet.type_or_length() == 0x0806
                    && ArpPacket::parse(ethernet.payload()).is_ok_and(|arp| {
                        arp.operation() == 1 && arp.target_protocol() == gateway.octets()
                    })
            })
            .count()
    };
    device.transmit_available.store(false, Ordering::Relaxed);
    poll(1000);
    assert_eq!(count(), 0);
    device.transmit_available.store(true, Ordering::Relaxed);
    for at in [1001, 2001, 3001, 4000] {
        poll(at);
    }
    assert_eq!(count(), 3);
    assert_eq!(rank(), (true, false));
    poll(4001);
    assert_eq!(rank(), (false, false));

    let mut reply = peer_arp_frame_for(local, gateway);
    let local_mac = [2, 0, 0, 0, 0, 1];
    reply[..6].copy_from_slice(&local_mac);
    reply[20..22].copy_from_slice(&2u16.to_be_bytes());
    reply[32..38].copy_from_slice(&local_mac);
    device.wire.lock().unwrap().0.push_back(reply);
    poll(4002);
    assert_eq!(rank(), (true, true));
    sender.close_description();
}

// 单独的 ARP 测试无法发现撤下主路由后停止复探。只向递归目标发送数据，验证切换实际出口。
#[test]
fn recursive_ipv4_route_switches_and_recovers_without_traffic_to_parent_prefix() {
    use core::sync::atomic::Ordering;
    use network_protocol::{ConfigurationBatch, NetworkInstaller};
    use packet_wire::{arp::ArpPacket, ethernet::EthernetFrame, ipv4::Ipv4Packet};
    use router_protocol::{
        configuration::Configuration,
        routing::{Batch, Family},
    };
    use routing_rib::sources::{CONFIGURATION_DOMAIN, Routes, STATIC_PROTOCOL};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let changes = Arc::new(crate::Event::default());
    observe_interfaces(&net, changes.clone()).unwrap();
    let primary_device = Arc::new(WireDevice::new());
    let backup_device = Arc::new(WireDevice::new());
    let primary_local = Ipv4Addr::new(10, 40, 0, 15);
    let backup_local = Ipv4Addr::new(10, 41, 0, 15);
    let primary = Ipv4Addr::new(10, 40, 0, 1);
    let backup = Ipv4Addr::new(10, 41, 0, 1);
    for (index, device, address) in [
        (2, &primary_device, primary_local),
        (3, &backup_device, backup_local),
    ] {
        assert_eq!(
            install_interface(
                &net,
                device.clone(),
                crate::Channel,
                crate::Event::default()
            )
            .unwrap(),
            index
        );
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }

    let config = Configuration::decode(br#"{
      "version":1,
      "interfaces":{
        "primary":{"device":{"kind":"name","value":"eth0"}},
        "backup":{"device":{"kind":"name","value":"eth1"}}
      },
      "routes":[
        {"destination":"198.51.100.9","prefix_length":32,"preference":10,"metric":100,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"primary","gateway":"10.40.0.1"}}]}},
        {"destination":"198.51.100.9","prefix_length":32,"preference":200,"metric":1,"action":{"kind":"forward","members":[{"target":{"kind":"direct","interface":"backup","gateway":"10.41.0.1"}}]}},
        {"destination":"203.0.113.9","prefix_length":32,"action":{"kind":"forward","members":[{"target":{"kind":"recursive","address":"198.51.100.9"}}]}}
      ]
    }"#).unwrap();
    let bytes = config
        .route_source(Family::Ipv4, |device| {
            Ok([(2, b"eth0"), (3, b"eth1")]
                .into_iter()
                .find_map(|(index, name)| device.matches(name, [0; 6]).then_some(index)))
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
    let install = |batch| {
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
    };
    let initial = routes
        .prepare_sources(&routing_facts(&net), Some(source), &[])
        .unwrap();
    install(ConfigurationBatch::parse(&initial.encoded).unwrap());
    routes = initial.next;

    let endpoint = scoped_endpoint(&net, InetProtocol::Udp);
    let destination = Ipv4Addr::new(203, 0, 113, 9);
    let peer = InetAddress::new(destination.into(), 44331);
    let send = || {
        endpoint
            .send_to(b"recursive route", Some(peer), &[])
            .unwrap()
    };
    let poll = |at| {
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(at))
    };
    let sent = |device: &WireDevice| {
        device
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|frame| {
                let ethernet = EthernetFrame::parse(frame).unwrap();
                ethernet.type_or_length() == 0x0800
                    && Ipv4Packet::parse(ethernet.payload())
                        .is_ok_and(|ip| ip.protocol() == 17 && ip.destination() == destination)
            })
            .count()
    };
    let probes = || {
        primary_device
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .filter(|frame| {
                let ethernet = EthernetFrame::parse(frame).unwrap();
                ethernet.type_or_length() == 0x0806
                    && ArpPacket::parse(ethernet.payload()).is_ok_and(|arp| {
                        arp.operation() == 1 && arp.target_protocol() == primary.octets()
                    })
            })
            .count()
    };
    let gateway_change = NetworkInstaller::GATEWAYS_CHANGED;
    let gateway_changed = || changes.0.load(Ordering::Relaxed) & gateway_change != 0;
    changes.signal(gateway_change, 0).unwrap();
    primary_device
        .wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(primary_local, primary));
    backup_device
        .wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(backup_local, backup));
    poll(1000);
    send();
    poll(1001);
    assert_eq!((sent(&primary_device), sent(&backup_device)), (1, 0));
    poll(50_000);
    send();
    poll(50_001);
    assert_eq!(sent(&primary_device), 2);
    for at in [55_001, 56_001, 57_001, 58_001] {
        poll(at);
    }
    assert_eq!(probes(), 3);
    assert!(gateway_changed());
    changes.signal(gateway_change, 0).unwrap();

    let failover = routes
        .prepare_sources(&routing_facts(&net), None, &[])
        .unwrap();
    let batch = ConfigurationBatch::parse(&failover.encoded).unwrap();
    assert!(batch.gateway_probes().eq([(2, primary.into())]));
    install(batch);
    routes = failover.next;
    send();
    poll(58_002);
    assert_eq!((sent(&primary_device), sent(&backup_device)), (2, 1));
    assert_eq!(probes(), 4);
    poll(118_001);
    assert_eq!(probes(), 4);
    poll(118_002);
    assert_eq!(probes(), 5);
    assert!(!gateway_changed());

    let mut reply = peer_arp_frame_for(primary_local, primary);
    let local_mac = [2, 0, 0, 0, 0, 1];
    reply[..6].copy_from_slice(&local_mac);
    reply[20..22].copy_from_slice(&2u16.to_be_bytes());
    reply[32..38].copy_from_slice(&local_mac);
    primary_device.wire.lock().unwrap().0.push_back(reply);
    poll(118_003);
    assert!(gateway_changed());
    let recovered = routes
        .prepare_sources(&routing_facts(&net), None, &[])
        .unwrap();
    let batch = ConfigurationBatch::parse(&recovered.encoded).unwrap();
    assert_eq!(batch.gateway_probes().len(), 0);
    install(batch);
    send();
    poll(118_004);
    assert_eq!((sent(&primary_device), sent(&backup_device)), (3, 1));
    endpoint.close_description();
}

#[test]
fn actual_arp_weak_host_and_duplicate_address_probe_share_fib_but_keep_physical_link() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89237);
    let local_b = Ipv4Addr::new(10, 41, 0, 15);
    // target属于B而ARP物理输入A；native arp_ignore=0按本域LOCAL回答。
    for source in [Ipv4Addr::new(198, 51, 100, 17), Ipv4Addr::UNSPECIFIED] {
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local_b, source));
        advance_actual_domains();
        assert!(a.wire.lock().unwrap().1.iter().any(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            if ethernet.ethertype() != EthernetProtocol::Arp { return false; }
            matches!(ArpRepr::parse(&ArpPacket::new_checked(ethernet.payload()).unwrap()).unwrap(),
                ArpRepr::EthernetIpv4 { operation: ArpOperation::Reply, source_protocol_addr, target_protocol_addr, .. }
                if source_protocol_addr == local_b && target_protocol_addr == source)
        }));
        assert!(
            b.wire.lock().unwrap().1.is_empty(),
            "ARP不是routed IP reply，必须留在原物理link"
        );
        a.wire.lock().unwrap().1.clear();
    }
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().address,
        Some(Ipv4Addr::new(10, 40, 0, 15))
    );
}
