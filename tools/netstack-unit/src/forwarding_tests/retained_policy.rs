use super::*;
use router_protocol::configuration::Configuration;

#[test]
fn policy_revisions_preserve_dual_stack_flows_and_rollback() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain_with_queues(1, 4);
    let addresses = [A, B].map(|address| {
        [router_protocol::configuration::Ipv6Address {
            address,
            prefix_length: 64,
            on_link: true,
        }]
    });
    let mut plans = [interface_plan(2, 40), interface_plan(3, 41)];
    for (plan, addresses) in plans.iter_mut().zip(&addresses) {
        plan.ipv6 = Some(router_protocol::interface::Ipv6Addresses::new(addresses).unwrap());
    }
    let setup = prepare_configuration(&net, plans.into_iter(), None).unwrap();
    setup.activate().unwrap();
    setup.confirm().unwrap();
    for now in [0, 1000, 2000, 2001] {
        poll(&net, now);
    }
    for (nic, local, peer, mac, subnet) in [(&a, A, FROM, FROM_MAC, 40), (&b, B, TO, TO_MAC, 41)] {
        inject(
            nic,
            neighbor(
                NdMessage::NeighborSolicitation { target: local },
                peer,
                local,
                mac,
            ),
        );
        inject(
            nic,
            peer_arp_frame_for(
                Ipv4Addr::new(10, subnet, 0, 15),
                Ipv4Addr::new(10, subnet, 0, 9),
            ),
        );
    }
    poll(&net, 2002);
    frames(&a);
    frames(&b);

    let stage = |preserve, port, reverse, protocol| {
        let (outside_v4, outside_v6) = if reverse {
            ("10.41.0.15", "2001:db8:29::f")
        } else {
            ("192.0.2.15", "2001:db8:32::f")
        };
        let mode = if preserve { "preserve" } else { "reevaluate" };
        let source = format!(
            r#"{{
            "version":1,
            "firewall":{{
                "existing_connections":"{mode}",
                "address_sets":{{"replies":[
                    {{"network":"{outside_v4}","length":32}},
                    {{"network":"{outside_v6}","length":128}}
                ]}},
                "prerouting":[
                    {{"source":{{"network":"10.40.0.0","length":24}},"transport":{{"protocol":"{protocol}"}},
                     "action":{{"kind":"source_nat","address":"10.41.0.15","port":{port}}}}},
                    {{"source":{{"network":"2001:db8:28::","length":64}},"transport":{{"protocol":"{protocol}"}},
                     "action":{{"kind":"source_nat","address":"2001:db8:29::f","port":{port}}}}},
                    {{"destination_set":"replies","connection_state":"established","action":{{"kind":"accept"}}}},
                    {{"action":{{"kind":"drop"}}}}
                ]
            }}
        }}"#
        );
        let config = Configuration::decode(source.as_bytes()).unwrap();
        let bytes = config.firewall_source(|_| Ok(None)).unwrap();
        let chain = router_protocol::firewall::Prerouting::parse(&bytes).unwrap();
        let batch = network_protocol::ConfigurationBatch::default().with_prerouting(chain);
        let update = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        update.activate().unwrap();
        update
    };
    let peers = [
        (
            Ipv4Addr::new(10, 40, 0, 9).into(),
            Ipv4Addr::new(10, 41, 0, 9).into(),
            Ipv4Addr::new(10, 41, 0, 15).into(),
        ),
        (FROM.into(), TO.into(), B.into()),
    ];
    let frame_for = |source, destination, protocol, payload: &[u8]| match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let mut packet = super::super::ipv4_forwarding::datagram(64, 0, &[], payload);
            packet_wire::ipv4::Ipv4Header {
                source,
                destination,
                protocol,
                hop_limit: 64,
                dscp_ecn: 0,
                identification: 0,
                fragment: 0,
                options: &[],
            }
            .emit(&mut packet[14..])
            .unwrap();
            packet
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            frame(source, destination, 64, protocol, payload)
        }
        _ => unreachable!(),
    };
    let send = |now, (client_port, remote_port, wire_port), reply, allowed| {
        let (input, output) = if reply { (&b, &a) } else { (&a, &b) };
        for (client, remote, outside) in peers {
            let (source, destination, source_port, destination_port) = if reply {
                (remote, outside, remote_port, wire_port)
            } else {
                (client, remote, client_port, remote_port)
            };
            let mut udp = [0; 12];
            packet_wire::udp::emit(&mut udp, source, destination, source_port, destination_port)
                .unwrap();
            let packet = frame_for(source, destination, 17, &udp);

            inject(input, packet);
        }
        poll(&net, now);
        let sent = frames(output);
        assert_eq!(sent.len(), if allowed { 2 } else { 0 }, "at {now}");
        for (frame, (client, remote, outside)) in sent.iter().zip(peers) {
            let (source, destination, bytes) = wire_payload(frame);
            let udp = packet_wire::udp::UdpPacket::parse(bytes, source, destination).unwrap();
            let expected = if reply {
                (remote, client, remote_port, client_port)
            } else {
                (outside, remote, wire_port, remote_port)
            };
            assert_eq!(
                (
                    source,
                    destination,
                    udp.source_port(),
                    udp.destination_port()
                ),
                expected
            );
        }
    };

    let first = (42_000, 5350, 45_000);
    let trial_only = (42_001, 5351, 45_001);
    let second = (42_002, 5352, 45_001);
    let third = (42_003, 5353, 45_002);
    stage(false, 45_000, true, "udp").confirm().unwrap();
    send(2003, first, false, true);
    send(2004, first, true, true);
    let trial = stage(true, 45_001, false, "udp");
    send(2005, first, true, true);
    send(2006, trial_only, false, true);
    send(2007, trial_only, true, false);
    trial.abort().unwrap();
    send(2008, trial_only, true, true);

    stage(true, 45_001, false, "udp").confirm().unwrap();
    send(2009, second, false, true);
    send(2010, second, true, false);
    stage(true, 45_002, true, "udp").confirm().unwrap();
    send(2011, first, false, true);
    send(2012, first, true, true);
    send(2013, second, true, false);
    send(2014, third, false, true);
    send(2015, third, true, true);

    let recheck = stage(false, 45_002, true, "udp");
    send(2016, second, true, true);
    recheck.abort().unwrap();
    send(2017, second, true, false);
    stage(false, 45_002, true, "udp").confirm().unwrap();
    send(2018, second, true, true);
    send(2019, first, false, true);

    stage(false, 45_100, false, "tcp").confirm().unwrap();
    send(2020, first, false, false);
    send(2021, first, true, false);

    use packet_wire::tcp::{self, TcpHeader, TcpPacket};
    let segment = |source, destination, ports: (u16, u16), flags, (sequence, acknowledgment)| {
        let mut bytes = [0; 20];
        TcpHeader {
            source_port: ports.0,
            destination_port: ports.1,
            sequence,
            acknowledgment,
            flags,
            window: 4096,
            urgent_pointer: 0,
        }
        .emit(&mut bytes, 20, source, destination)
        .unwrap();
        frame_for(source, destination, 6, &bytes)
    };
    let syns = |now, sequence, port| {
        for (client, remote, _) in peers {
            inject(
                &a,
                segment(client, remote, (42_010, 5354), tcp::SYN, (sequence, 0)),
            );
        }
        poll(&net, now);
        let sent = frames(&b);
        assert_eq!(sent.len(), 2);
        for (frame, (_, remote, outside)) in sent.iter().zip(peers) {
            let (source, destination, bytes) = wire_payload(frame);
            let tcp = TcpPacket::parse(bytes, source, destination)
                .unwrap()
                .header();
            assert_eq!(
                (source, destination, tcp.source_port(), tcp.sequence()),
                (outside, remote, port, sequence)
            );
        }
    };
    stage(false, 45_100, true, "tcp").confirm().unwrap();
    syns(2022, 100, 45_100);
    for (_, remote, outside) in peers {
        inject(
            &b,
            segment(
                remote,
                outside,
                (5354, 45_100),
                tcp::RST | tcp::ACK,
                (0, 101),
            ),
        );
    }
    poll(&net, 2023);
    assert_eq!(frames(&a).len(), 2);
    stage(true, 45_101, true, "tcp").confirm().unwrap();
    syns(2024, 200, 45_101);
}
