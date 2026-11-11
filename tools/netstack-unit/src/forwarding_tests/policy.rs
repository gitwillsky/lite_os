use super::super::ipv4_forwarding;
use super::*;

#[test]
fn policy_routing_selects_actual_dual_stack_egress_and_rolls_back() {
    use network_protocol::routes::{RouteDirection, RouteLookup};
    use router_protocol::firewall::counters::{Cursor, Page};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89407);
    let counters = |cursor| {
        let page = crate::inet::firewall_counters(&net, cursor).unwrap();
        assert_eq!(Page::parse(&page.encode().unwrap(), cursor).unwrap(), page);
        page
    };
    let first_page = Cursor::default();
    let addresses = [A, B].map(|address| {
        [router_protocol::configuration::Ipv6Address {
            address,
            prefix_length: 64,
            on_link: true,
        }]
    });
    let mut plans = [interface_plan(2, 40), interface_plan(3, 41)];
    for (plan, address) in plans.iter_mut().zip(&addresses) {
        plan.ipv6 = Some(router_protocol::interface::Ipv6Addresses::new(address).unwrap());
    }
    let configured = prepare_configuration(&net, plans.into_iter(), None).unwrap();
    configured.activate().unwrap();
    configured.confirm().unwrap();
    for now in [0, 1000, 2000, 2001] {
        poll(&net, now);
    }
    for (nic, local, peer, mac) in [(&a, A, FROM, FROM_MAC), (&b, B, TO, TO_MAC)] {
        inject(
            nic,
            neighbor(
                NdMessage::NeighborSolicitation { target: local },
                peer,
                local,
                mac,
            ),
        );
    }
    inject(
        &a,
        peer_arp_frame_for(Ipv4Addr::new(10, 40, 0, 15), Ipv4Addr::new(10, 40, 0, 9)),
    );
    inject(
        &b,
        peer_arp_frame_for(Ipv4Addr::new(10, 41, 0, 15), Ipv4Addr::new(10, 41, 0, 9)),
    );
    poll(&net, 2002);
    frames(&a);
    frames(&b);

    let mut config = router_protocol::configuration::Configuration::decode(br#"{
        "version": 1,
        "interfaces": {"a": {"device": {"kind": "name", "value": "a"}}},
        "firewall": {
            "prerouting": [
                {"ingress": "a", "destination": "10.41.0.9", "prefix_length": 32, "action": {"kind": "set_mark", "value": 99}},
                {"ingress": "a", "destination": "2001:db8:29::9", "prefix_length": 128, "action": {"kind": "set_mark", "value": 99}},
                {"destination": "10.41.0.0", "prefix_length": 24, "action": {"kind": "set_mark", "value": 99}},
                {"destination": "2001:db8:29::", "prefix_length": 64, "action": {"kind": "set_mark", "value": 99}},
                {"ingress": "a", "destination": "10.41.0.0", "prefix_length": 24, "action": {"kind": "set_mark", "value": 42}},
                {"ingress": "a", "destination": "2001:db8:29::", "prefix_length": 64, "action": {"kind": "set_mark", "value": 42}}
            ]
        },
        "policy_routing": {
            "tables": [
                {"id": 200, "routes": [
                    {"destination": "10.41.0.0", "prefix_length": 24, "action": {"kind": "forward",
                        "members": [{"target": {"kind": "direct", "interface": "a", "gateway": "10.40.0.9"}}]}},
                    {"destination": "2001:db8:29::", "prefix_length": 64, "action": {"kind": "forward",
                        "members": [{"target": {"kind": "direct", "interface": "a", "gateway": "2001:db8:28::9"}}]}}
                ]},
                {"id": 100}
            ],
            "rules": [
                {"priority": 18, "source": "10.41.0.15", "prefix_length": 32, "origin": "local", "mark": 2147483648, "mark_mask": 2147483648, "table": 200},
                {"priority": 19, "source": "2001:db8:29::f", "prefix_length": 128, "origin": "local", "mark": 2147483648, "mark_mask": 2147483648, "table": 200},
                {"priority": 21, "source": "2001:db8:28::9", "prefix_length": 128, "ingress": "a", "mark": 42, "table": 200},
                {"priority": 20, "source": "10.40.0.9", "prefix_length": 32, "ingress": "a", "mark": 42, "table": 200},
                {"priority": 11, "source": "::", "prefix_length": 0, "table": 100},
                {"priority": 10, "source": "0.0.0.0", "prefix_length": 0, "table": 100}
            ]
        }
    }"#).unwrap();
    let facts = routing_facts(&net);
    let stage = |rib: &routing_rib::sources::Routes,
                 config: &router_protocol::configuration::Configuration| {
        let tables = config.policy_source(|_| Ok(Some(2))).unwrap();
        let intent = network_protocol::routing::Configuration::encode(
            &[],
            [None, None],
            None,
            Some(&tables),
        )
        .unwrap();
        let input = network_protocol::routing::Configuration::parse(&intent).unwrap();
        let prepared = rib
            .prepare_configuration(&facts, &[], &[], None, input.tables())
            .unwrap();
        let firewall = config.firewall_source(|_| Ok(Some(2))).unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded)
            .unwrap()
            .with_prerouting(router_protocol::firewall::Prerouting::parse(&firewall).unwrap());
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        (prepared.next, transaction)
    };
    let (rib, transaction) = stage(&routing_rib::sources::Routes::default(), &config);
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let rules = crate::inet::configuration::rule_snapshot(&net, None).unwrap();
    assert!(
        (0..rules.len())
            .map(|index| rules.get(index).unwrap().rule.priority)
            .eq([0, 10, 18, 20, 32766, 0, 11, 19, 21, 32766])
    );
    let only6 =
        crate::inet::configuration::rule_snapshot(&net, Some(network_protocol::InetFamily::Ipv6))
            .unwrap();
    assert_eq!(only6.len(), 5);
    assert_eq!(only6.get(3), rules.get(8));
    for index in [2, 7] {
        assert_eq!(
            rules.get(index).unwrap().ingress(),
            Some(b"lo\0".as_slice())
        );
    }
    let flows = [
        (
            Ipv4Addr::new(10, 40, 0, 9).into(),
            Ipv4Addr::new(10, 41, 0, 9).into(),
        ),
        (FROM.into(), TO.into()),
    ];
    let lookup = |source, destination, ingress, mark| {
        crate::inet::configuration::route_lookup(
            &net,
            RouteLookup {
                source,
                destination,
                direction: RouteDirection::Input(ingress),
                mark,
            },
        )
    };
    for ((source, destination), index) in flows.into_iter().zip([3, 8]) {
        let path = lookup(source, destination, 2, 42).unwrap();
        assert_eq!(
            (path.table, path.interface_index, path.source, path.gateway),
            (200, 2, source, Some(source))
        );
        let rule = rules.get(index).unwrap();
        assert_eq!(
            (rule.rule.source, rule.rule.table, rule.ingress()),
            (source, path.table, Some(b"eth0\0".as_slice()))
        );
        let main = lookup(source, destination, 3, 42).unwrap();
        assert_eq!(
            (main.table, main.interface_index, main.source, main.gateway),
            (254, 3, source, None)
        );
        assert_eq!(lookup(source, destination, 2, 0).unwrap().table, 254);
        assert_eq!(
            lookup(source, destination, 99, 42),
            Err(SocketError::NoDevice)
        );
    }
    let no_source = lookup(Ipv6Addr::UNSPECIFIED.into(), TO.into(), 2, 42).unwrap();
    assert_eq!((no_source.table, no_source.interface_index), (254, 3));
    assert!(no_source.source.is_unspecified());
    let paused = prepare_configuration(
        &net,
        core::iter::empty(),
        Some(network_protocol::Forwarding {
            ipv4: false,
            ipv6: false,
        }),
    )
    .unwrap();
    paused.activate().unwrap();
    for ((source, destination), local) in flows
        .into_iter()
        .zip([Ipv4Addr::new(10, 40, 0, 15).into(), A.into()])
    {
        assert_eq!(
            lookup(source, destination, 2, 42),
            Err(SocketError::HostUnreachable)
        );
        assert_eq!(
            lookup(source, local, 2, 42).unwrap().kind,
            network_protocol::RouteKind::Local
        );
    }
    paused.abort().unwrap();

    let mark = 0x8000_0042;
    let output_lookup = |source, destination, outgoing, mark| {
        crate::inet::configuration::route_lookup(
            &net,
            RouteLookup {
                source,
                destination,
                direction: RouteDirection::Output(outgoing),
                mark,
            },
        )
    };
    let outputs = [
        (
            Ipv4Addr::UNSPECIFIED.into(),
            Ipv4Addr::new(10, 41, 0, 15).into(),
            flows[0].1,
            network_protocol::InetFamily::Ipv4,
        ),
        (
            Ipv6Addr::UNSPECIFIED.into(),
            B.into(),
            TO.into(),
            network_protocol::InetFamily::Ipv6,
        ),
    ];
    for (unspecified, source, destination, family) in outputs {
        let ordinary = output_lookup(unspecified, destination, None, 0).unwrap();
        assert_eq!((ordinary.table, ordinary.interface_index), (254, 3));
        let path = output_lookup(unspecified, destination, None, mark).unwrap();
        assert_eq!(
            (path.table, path.interface_index, path.source),
            (200, 2, source)
        );
        assert_eq!(
            output_lookup(source, destination, None, mark).unwrap(),
            path
        );
        let socket = InetSocket::new(
            family,
            network_protocol::InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        assert_eq!(socket.option(network_protocol::InternetOption::Mark), Ok(0));
        socket.set_mark(mark).unwrap();
        assert_eq!(
            socket.option(network_protocol::InternetOption::Mark),
            Ok(mark as i32)
        );
        socket
            .send_to(
                b"local policy",
                Some(InetAddress::new(destination, 4567)),
                &[],
            )
            .unwrap();
        socket.set_mark(0).unwrap();
        poll(&net, 2002);
        let sent = frames(&a)
            .into_iter()
            .find_map(|frame| match [frame[12], frame[13]] {
                [0x08, 0x00] if destination.is_ipv4() && frame[23] == 17 => {
                    let packet = packet_wire::ipv4::Ipv4Packet::parse(&frame[14..]).unwrap();
                    Some((packet.source().into(), packet.destination().into()))
                }
                [0x86, 0xdd] if destination.is_ipv6() && ip(&frame).upper_protocol() == 17 => {
                    let packet = ip(&frame);
                    Some((packet.source().into(), packet.destination().into()))
                }
                _ => None,
            });
        assert_eq!(sent, Some((source, destination)));
        assert!(frames(&b).is_empty());
        socket.close_description();
    }
    let send = |input, now| {
        inject(input, ipv4_forwarding::datagram(64, 0, &[], b"policy"));
        inject(input, frame(FROM, TO, 64, 253, b"policy"));
        poll(&net, now);
    };
    let transit = |nic: &WireDevice| {
        frames(nic)
            .into_iter()
            .filter(|frame| match [frame[12], frame[13]] {
                [0x08, 0x00] => frame[23] == 253,
                [0x86, 0xdd] => ip(frame).upper_protocol() == 253,
                _ => false,
            })
            .count()
    };
    // 较早的空表允许后续匹配；指定入接口生效时，两族都从备选表的网关发出。
    send(&a, 2003);
    assert_eq!((transit(&a), transit(&b)), (2, 0));
    send(&b, 2004);
    assert_eq!((transit(&a), transit(&b)), (0, 2));

    let mut other4 = ipv4_forwarding::datagram(64, 0, &[], b"other source");
    other4[29] = 10;
    other4[24..26].fill(0);
    let checksum = packet_wire::checksum::checksum(&other4[14..34]);
    other4[24..26].copy_from_slice(&checksum.to_be_bytes());
    inject(&a, other4);
    inject(
        &a,
        frame(
            "2001:db8:28::a".parse().unwrap(),
            TO,
            64,
            253,
            b"other source",
        ),
    );
    poll(&net, 2005);
    assert_eq!((transit(&a), transit(&b)), (0, 2));

    let empty = router_protocol::configuration::Configuration::default()
        .firewall_source(|_| Ok(None))
        .unwrap();
    let firewall_only = network_protocol::ConfigurationBatch::default()
        .with_prerouting(router_protocol::firewall::Prerouting::parse(&empty).unwrap());
    use core::sync::atomic::Ordering::Relaxed;
    a.transmit_available.store(false, Relaxed);
    send(&a, 2005);
    assert_eq!((transit(&a), transit(&b)), (0, 0));
    let queued = counters(first_page);
    let pending = crate::inet::prepare_configuration(&net, Some(firewall_only), None).unwrap();
    pending.activate().unwrap();
    send(&a, 2005);
    assert_eq!((transit(&a), transit(&b)), (0, 2));
    // 已通过入站链的旧包不能在退避结束后被新链重新标记。
    a.transmit_available.store(true, Relaxed);
    a.transmit_ready.store(true, Relaxed);
    poll(&net, 2005);
    assert_eq!((transit(&a), transit(&b)), (2, 0));
    assert!(counters(first_page).rows.is_empty());
    pending.abort().unwrap();
    assert_eq!(counters(first_page), queued);
    send(&a, 2005);
    assert_eq!((transit(&a), transit(&b)), (2, 0));

    // 提前放行须保留其前面的标记，并跳过其后的赋标及丢弃；命中丢弃时后续放行无效。
    use router_protocol::configuration::{PreroutingAction, PreroutingRule};
    let previous = core::mem::take(&mut config.firewall);
    let ingress = config.interfaces.keys().next().unwrap();
    for (network, prefix, destination, width) in [
        ("10.41.0.0", 24, flows[0].1, 32),
        ("2001:db8:29::", 64, flows[1].1, 128),
    ] {
        let network = network.parse().unwrap();
        for (interface, destination, prefix_length, action) in [
            (
                None,
                network,
                prefix,
                PreroutingAction::SetMark { value: 99 },
            ),
            (
                Some(ingress),
                network,
                prefix,
                PreroutingAction::SetMark { value: 42 },
            ),
            (Some(ingress), destination, width, PreroutingAction::Accept),
            (
                Some(ingress),
                network,
                prefix,
                PreroutingAction::SetMark { value: 99 },
            ),
            (
                None,
                network,
                prefix,
                PreroutingAction::SetMark { value: 98 },
            ),
            (None, network, prefix, PreroutingAction::Drop),
            (None, network, prefix, PreroutingAction::Accept),
        ] {
            config.firewall.prerouting.push(PreroutingRule {
                ingress: interface.cloned(),
                destination: Some(destination),
                prefix_length: Some(prefix_length),
                destination_set: None,
                action,
                connection_state: None,
            });
        }
    }
    let filter = config.firewall_source(|_| Ok(Some(2))).unwrap();
    let filter = network_protocol::ConfigurationBatch::default()
        .with_prerouting(router_protocol::firewall::Prerouting::parse(&filter).unwrap());
    let filtering = crate::inet::prepare_configuration(&net, Some(filter), None).unwrap();
    filtering.activate().unwrap();
    send(&a, 2005);
    assert_eq!((transit(&a), transit(&b)), (2, 0));
    send(&b, 2005);
    assert!(frames(&a).is_empty() && frames(&b).is_empty());
    let hits = counters(first_page);
    assert_eq!(hits.rows.len(), 14);
    for (rows, length) in hits.rows.as_chunks::<7>().0.iter().zip([26, 46]) {
        for (counter, packets) in rows.iter().zip([2, 1, 1, 0, 1, 1, 0]) {
            assert_eq!((counter.packets, counter.bytes), (packets, packets * length));
        }
    }
    filtering.abort().unwrap();
    send(&b, 2005);
    assert_eq!((transit(&a), transit(&b)), (0, 2));
    config.firewall = previous;

    for route in &mut config.policy_routing.tables[0].routes {
        route.action = router_protocol::configuration::StaticAction::Prohibit;
    }
    let (_, transaction) = stage(&rib, &config);
    transaction.activate().unwrap();
    assert_eq!(counters(first_page).revision, queued.revision);
    for (unspecified, source, destination, _) in outputs {
        assert_eq!(
            output_lookup(unspecified, destination, None, mark),
            Err(SocketError::PermissionDenied)
        );
        assert_eq!(
            output_lookup(source, destination, Some(3), mark),
            Err(SocketError::PermissionDenied)
        );
    }
    for (source, destination) in flows {
        assert_eq!(
            lookup(source, destination, 2, 42),
            Err(SocketError::PermissionDenied)
        );
    }
    send(&a, 2006);
    assert_eq!(
        transit(&b),
        0,
        "a policy rejection must not fall back to main"
    );
    let replies = frames(&a);
    assert!(
        replies.iter().any(|frame| frame[12..14] == [0x08, 0x00]
            && frame[23] == 1
            && frame[34..36] == [3, 13])
    );
    assert!(replies.iter().any(|frame| frame[12..14] == [0x86, 0xdd]
        && ip(frame).upper_protocol() == 58
        && frame[54..56] == [1, 1]));
    transaction.abort().unwrap();
    for (source, destination) in flows {
        assert_eq!(lookup(source, destination, 2, 42).unwrap().table, 200);
    }
    send(&a, 2007);
    assert_eq!((transit(&a), transit(&b)), (2, 0));

    config.policy_routing = Default::default();
    let (_, transaction) = stage(&rib, &config);
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let current_rules = crate::inet::configuration::rule_snapshot(&net, None).unwrap();
    assert_eq!(current_rules.len(), 4);
    assert_eq!(
        rules.len(),
        10,
        "the earlier dump keeps the captured generation"
    );
    for (source, destination) in flows {
        assert_eq!(lookup(source, destination, 2, 42).unwrap().table, 254);
    }
    for (source, _, destination, _) in outputs {
        assert_eq!(
            output_lookup(source, destination, None, mark)
                .unwrap()
                .table,
            254
        );
    }
    send(&a, 2008);
    assert_eq!((transit(&a), transit(&b)), (0, 2));

    // 重叠成员只执行一次同一规则。两个地址族和两条规则共用同一集合。
    let source = r#"{"version":1,"firewall":{
        "address_sets":{"blocked":[
            {"network":"10.41.0.0","length":24},{"network":"10.41.0.9","length":32},
            {"network":"2001:db8:29::","length":64},{"network":"2001:db8:29::9","length":128}
        ]},
        "prerouting":[
            {"destination_set":"blocked","action":{"kind":"set_mark","value":42}},
            {"destination_set":"blocked","action":{"kind":"drop"}},
            {"destination":"10.41.0.9","prefix_length":32,"action":{"kind":"accept"}}
        ]}}"#;
    use router_protocol::configuration::Configuration;
    assert!(Configuration::decode(source.replacen("\"destination_set\":\"blocked\"", "\"destination_set\":\"undefined\"", 1).as_bytes()).is_err());
    let mut sets = Configuration::decode(source.as_bytes()).unwrap();
    let prepare_firewall = |configuration: &Configuration| {
        let bytes = configuration.firewall_source(|_| Ok(None)).unwrap();
        let chain = router_protocol::firewall::Prerouting::parse(&bytes).unwrap();
        let batch = network_protocol::ConfigurationBatch::default().with_prerouting(chain);
        crate::inet::prepare_configuration(&net, Some(batch), None).unwrap()
    };
    let install = prepare_firewall(&sets);
    install.activate().unwrap();
    install.confirm().unwrap();
    send(&a, 2009);
    assert_eq!((transit(&a), transit(&b)), (0, 0));
    let active = counters(first_page);
    assert_eq!(active.rows.iter().map(|row| row.packets).collect::<Vec<_>>(), [2, 2, 0]);
    assert_eq!(active.rows[0].destination_set.as_deref(), Some("blocked"));
    let classification = || crate::inet::stack(&net).unwrap().lock().unwrap().firewall.classification().unwrap();
    let original = classification();

    sets.firewall.address_sets.values_mut().next().unwrap().retain(|prefix| prefix.network.is_ipv6());
    let update = prepare_firewall(&sets);
    update.activate().unwrap();
    assert!(Arc::ptr_eq(&original, &classification()));
    send(&a, 2010);
    assert_eq!((transit(&a), transit(&b)), (0, 1));
    let updated = counters(first_page);
    assert_ne!(updated.revision, active.revision);
    assert_eq!(updated.rows.iter().map(|row| row.packets).collect::<Vec<_>>(), [3, 3, 1]);
    update.abort().unwrap();
    send(&a, 2011);
    assert_eq!((transit(&a), transit(&b)), (0, 0));
    let rolled_back = counters(first_page);
    assert_eq!(rolled_back.revision, active.revision);
    assert_eq!(rolled_back.rows.iter().map(|row| row.packets).collect::<Vec<_>>(), [5, 5, 1]);
}
