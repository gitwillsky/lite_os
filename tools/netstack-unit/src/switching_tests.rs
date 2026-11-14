// L2 configuration, ingress classification, MAC move and output tag formatting
// must agree on actual NIC frames. Packet codec tests cannot verify this path.
#[test]
fn vlan_bridge_forwards_learns_moves_and_restores_live_port_policy() {
    use packet_wire::ethernet::{CUSTOMER_TAG, EthernetFrame, EthernetHeader, VlanTag};
    use router_protocol::configuration::Configuration;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    let [a, b, c] = core::array::from_fn(|_| Arc::new(WireDevice::new()));
    net.install_protocol(prepare_network(&net, &[a.clone(), b.clone(), c.clone()], true).unwrap());
    for index in 2..=4 {
        configure_up(&net, index, true).unwrap();
    }

    let mut configuration = Configuration::decode(
        br#"{
      "version":1,
      "interfaces":{
        "access":{"device":{"kind":"name","value":"eth0"}},
        "trunk":{"device":{"kind":"name","value":"eth1"}},
        "other":{"device":{"kind":"name","value":"eth2"}}
      },
      "bridges":{"lan":{"rstp":false,"ports":{
        "access":{"pvid":10,"vlans":{"10":"untagged"}},
        "trunk":{"vlans":{"10":"tagged","20":"tagged"}},
        "other":{"pvid":20,"vlans":{"10":"untagged","20":"untagged"}}
      }}}
    }"#,
    )
    .unwrap();
    let stage = |configuration: &Configuration| {
        let bytes = configuration
            .bridge_source(|device| {
                Ok([(b"eth0", 2), (b"eth1", 3), (b"eth2", 4)]
                    .into_iter()
                    .find_map(|(name, index)| {
                        device.matches(name, [2, 0, 0, 0, 0, 1]).then_some(index)
                    }))
            })
            .unwrap();
        let ports = router_protocol::switching::Ports::parse(&bytes).unwrap();
        let batch = network_protocol::ConfigurationBatch::default().with_bridges(ports);
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction
    };
    stage(&configuration).confirm().unwrap();
    let addr = |suffix| [2, 9, 8, 7, 6, suffix];
    let put = |device: &WireDevice, source, destination, tci: Option<u16>, size: usize| {
        let tag = [VlanTag {
            protocol: CUSTOMER_TAG,
            control: tci.unwrap_or(0),
        }];
        let header = EthernetHeader {
            source,
            destination,
            type_or_length: 0x88b5,
            tags: if tci.is_some() { &tag } else { &[] },
        };
        let mut frame = vec![0x52; header.length() + size];
        header.emit(&mut frame).unwrap();
        device.wire.lock().unwrap().0.push_back(frame);
    };
    let poll = |ms| {
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    };
    let drain = |device: &WireDevice| core::mem::take(&mut device.wire.lock().unwrap().1);

    put(&a, addr(1), addr(7), None::<u16>, 1500);
    poll(1000);
    let tagged = drain(&b);
    let untagged = drain(&c);
    assert_eq!((tagged.len(), untagged.len()), (1, 1));
    assert_eq!((tagged[0].len(), untagged[0].len()), (1518, 1514));
    let frame = EthernetFrame::parse(&tagged[0]).unwrap();
    let network = frame.network().unwrap();
    assert_eq!(network.tags().next().unwrap().control, 10);
    assert_eq!(network.payload(), &[0x52; 1500]);

    // The learned address is scoped to VLAN 10. A VLAN 20 flood cannot reach access.
    put(&b, addr(2), addr(1), Some(20), 32);
    poll(1001);
    assert!(drain(&a).is_empty());
    assert_eq!(drain(&c).len(), 1);
    put(&c, addr(1), addr(2), Some(10), 32);
    poll(1002);
    drain(&a);
    drain(&b);
    put(&b, addr(2), addr(1), Some(10), 32);
    poll(1003);
    assert!(drain(&a).is_empty(), "the address moved to the other port");
    assert_eq!(drain(&c).len(), 1);

    configuration
        .bridges
        .values_mut()
        .next()
        .unwrap()
        .ports
        .values_mut()
        .find(|port| port.pvid == Some(20))
        .unwrap()
        .vlans
        .remove(&10);
    let change = stage(&configuration);
    put(&b, addr(2), addr(1), Some(10), 32);
    poll(1004);
    assert_eq!(drain(&a).len(), 1);
    assert!(drain(&c).is_empty());
    change.abort().unwrap();
    put(&b, addr(2), addr(1), Some(10), 32);
    poll(1005);
    assert!(
        drain(&a).is_empty(),
        "abort restores the prior learned address"
    );
    assert_eq!(drain(&c).len(), 1);

    let deadline = |ms| {
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .switching
            .next_deadline(Instant::from_millis(ms))
            .map(|deadline| deadline.total_millis())
    };
    assert_eq!(deadline(1005), Some(301000));
    put(&b, addr(2), addr(1), Some(10), 32);
    poll(301000);
    assert_eq!((drain(&a).len(), drain(&c).len()), (0, 1));
    assert_eq!(deadline(301000), Some(301001));
    put(&b, addr(2), addr(1), Some(10), 32);
    poll(301002);
    assert_eq!((drain(&a).len(), drain(&c).len()), (1, 1));

    let bridge = configuration.bridges.values_mut().next().unwrap();
    for port in bridge.ports.values_mut().filter(|port| port.pvid.is_some()) {
        port.vlans
            .insert(10, router_protocol::configuration::VlanEgress::Untagged);
        port.isolated = true;
    }
    stage(&configuration).confirm().unwrap();
    put(&a, addr(1), [0xff; 6], Some(0xa000), 32);
    poll(301003);
    assert!(
        drain(&c).is_empty(),
        "isolated ports do not exchange broadcasts"
    );
    let trunk = drain(&b);
    assert_eq!(trunk.len(), 1);
    let frame = EthernetFrame::parse(&trunk[0]).unwrap().network().unwrap();
    assert_eq!(frame.tags().next().unwrap().control, 0xa00a);

    put(&a, addr(1), [1, 0x80, 0xc2, 0, 0, 0], None, 32);
    poll(301004);
    assert!(
        drain(&b).is_empty(),
        "bridge-local control addresses are not flooded"
    );

    for suffix in 20..36 {
        put(&a, addr(suffix), addr(7), None, 32);
    }
    poll(301004);
    let due = 601004;
    assert!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_until(Instant::from_millis(due), 0)
            .backlog
    );
    assert_eq!(deadline(due), Some(due));
    poll(due);
    assert_eq!(deadline(due), None);
}

// A wire-level cycle catches failures that testing the selected root alone cannot:
// both bridge links may carry a frame, but the destination LAN must see it once.
#[test]
fn rstp_bridge_blocks_a_redundant_link_and_uses_it_after_carrier_loss() {
    use packet_wire::ethernet::{EthernetFrame, EthernetHeader};
    use packet_wire::spanning_tree::{self, Bpdu};
    use router_protocol::configuration::Configuration;

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let setup = |priority| {
        let net = crate::namespace::Namespace::create().unwrap();
        let [primary, reserve, station] = core::array::from_fn(|_| Arc::new(WireDevice::new()));
        net.install_protocol(
            prepare_network(
                &net,
                &[primary.clone(), reserve.clone(), station.clone()],
                true,
            )
            .unwrap(),
        );
        for index in 2..=4 {
            configure_up(&net, index, true).unwrap();
        }
        let mut config = Configuration::decode(
            br#"{
          "version":1,
          "interfaces":{
            "primary":{"device":{"kind":"name","value":"eth0"}},
            "reserve":{"device":{"kind":"name","value":"eth1"}},
            "station":{"device":{"kind":"name","value":"eth2"}}
          },
          "bridges":{"lan":{"ports":{
            "primary":{"pvid":1,"vlans":{"1":"untagged"}},
            "reserve":{"pvid":1,"vlans":{"1":"untagged"}},
            "station":{"edge":true,"pvid":1,"vlans":{"1":"untagged"}}
          }}}
        }"#,
        )
        .unwrap();
        config.bridges.values_mut().next().unwrap().priority = priority;
        let bytes = config
            .bridge_source(|device| {
                Ok([(b"eth0", 2), (b"eth1", 3), (b"eth2", 4)]
                    .into_iter()
                    .find_map(|(name, index)| {
                        device.matches(name, [2, 0, 0, 0, 0, 1]).then_some(index)
                    }))
            })
            .unwrap();
        let ports = router_protocol::switching::Ports::parse(&bytes).unwrap();
        let batch = network_protocol::ConfigurationBatch::default().with_bridges(ports);
        let transaction = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        transaction.activate().unwrap();
        transaction.confirm().unwrap();
        (net, [primary, reserve, station])
    };
    let (a, [a_primary, a_reserve, a_station]) = setup(32768);
    let (b, [b_primary, b_reserve, b_station]) = setup(36864);
    let drain = |device: &WireDevice| core::mem::take(&mut device.wire.lock().unwrap().1);
    let transfer = |source: &WireDevice, target: &WireDevice| {
        let mut data_frames = 0;
        let frames = drain(source);
        for bytes in &frames {
            let frame = EthernetFrame::parse(bytes).unwrap();
            if frame.destination() == spanning_tree::DESTINATION {
                assert_eq!(bytes.len(), 60);
                assert_eq!(frame.type_or_length(), 39);
                assert!(matches!(
                    Bpdu::parse(frame.network().unwrap().payload()),
                    Ok(Bpdu::Configuration(message)) if message.rapid
                ));
            } else {
                data_frames += 1;
            }
        }
        target.wire.lock().unwrap().0.extend(frames);
        data_frames
    };
    let poll = |net: &crate::namespace::Namespace, ms| {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    };
    let send = |station: &WireDevice, source: [u8; 6], destination: [u8; 6]| {
        let mut frame = vec![0x5a; 60];
        EthernetHeader {
            source,
            destination,
            type_or_length: 0x88b5,
            tags: &[],
        }
        .emit(&mut frame)
        .unwrap();
        station.wire.lock().unwrap().0.push_back(frame);
    };
    let received = |station: &WireDevice| {
        drain(station)
            .into_iter()
            .filter(|bytes| EthernetFrame::parse(bytes).unwrap().type_or_length() == 0x88b5)
            .count()
    };
    let drive = |ms| {
        poll(&a, ms);
        let sent = (
            transfer(&a_primary, &b_primary),
            transfer(&a_reserve, &b_reserve),
        );
        poll(&b, ms);
        let returned = (
            transfer(&b_primary, &a_primary),
            transfer(&b_reserve, &a_reserve),
        );
        (sent, returned)
    };

    // The handshake must finish long before the two 15-second forwarding delays.
    for ms in (1000..1600).step_by(50) {
        drive(ms);
    }
    let left = [2, 9, 8, 7, 6, 1];
    let right = [2, 9, 8, 7, 6, 2];
    send(&a_station, left, [0xff; 6]);
    assert_eq!(drive(1600), ((1, 1), (0, 0)));
    assert_eq!(received(&b_station), 1);
    send(&b_station, right, left);
    assert_eq!(drive(1650).1, (1, 0));
    drive(1700);
    assert_eq!(received(&a_station), 1);

    a_primary.set_carrier(false);
    b_primary.set_carrier(false);
    for ms in (1750..2000).step_by(50) {
        drive(ms);
    }
    send(&a_station, left, right);
    assert_eq!(drive(2000), ((0, 1), (0, 0)));
    assert_eq!(received(&b_station), 1);
    send(&b_station, right, left);
    assert_eq!(drive(2050).1, (0, 1));
    drive(2100);
    assert_eq!(received(&a_station), 1);

    // Inject the seven-byte LLC TCN of an old STP peer. The response must use
    // the classic frame type and notify that link of the topology change.
    a_primary.set_carrier(true);
    let mut tcn = [0; 60];
    let at = EthernetHeader {
        source: right,
        destination: spanning_tree::DESTINATION,
        type_or_length: 7,
        tags: &[],
    }
    .emit(&mut tcn)
    .unwrap();
    tcn[at..at + 7].copy_from_slice(&[0x42, 0x42, 3, 0, 0, 0, 0x80]);
    a_primary.wire.lock().unwrap().0.push_back(tcn.to_vec());
    poll(&a, 2150);
    let frames = drain(&a_primary);
    let reply = EthernetFrame::parse(frames.last().unwrap()).unwrap();
    let payload = reply.network().unwrap().payload();
    assert_eq!(reply.type_or_length(), 38);
    assert_eq!(&payload[..8], &[0x42, 0x42, 3, 0, 0, 0, 0, 0x81]);
}

// 接口配置与 Ethernet 网桥各自测试无法发现跨 VLAN 时的逻辑入接口和错误封装。
#[test]
fn bridge_vlan_gateways_route_both_families_and_rollback_interface_removal() {
    use network_protocol::routes::{RouteDirection, RouteLookup};
    use packet_wire::{
        ethernet::{CUSTOMER_TAG, EthernetFrame, EthernetHeader, VlanTag},
        icmpv6::Icmpv6Packet,
        ipv4::Ipv4Packet,
        ipv6::Ipv6Packet,
        ndisc::{NdMessage, NdPacket, options::OptionValue},
    };
    use router_protocol::{
        configuration::{Configuration, InterfaceReference, RouterAdvertisements},
        routing::{Batch, Family},
        switching::Ports,
    };
    use routing_rib::sources::{CONFIGURATION_DOMAIN, Routes, STATIC_PROTOCOL};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    let [access, trunk] = core::array::from_fn(|_| Arc::new(WireDevice::new()));
    net.install_protocol(prepare_network(&net, &[access.clone(), trunk.clone()], true).unwrap());
    for index in [2, 3] {
        configure_up(&net, index, true).unwrap();
    }
    let mut config = Configuration::decode(
        br#"{
      "version":1,
      "interfaces":{
        "access":{"device":{"kind":"name","value":"eth0"}},
        "trunk":{"device":{"kind":"name","value":"eth1"}}
      },
      "bridges":{"lan":{"rstp":false,"ports":{
        "access":{"pvid":10,"vlans":{"10":"untagged"}},
        "trunk":{"vlans":{"20":"tagged"}}
      },"gateways":{
        "10":{"name":"lan10","address":"10.40.0.15","prefix_length":24,
              "ipv6":[{"address":"2001:db8:28::f","prefix_length":64}]},
        "20":{"name":"lan20","address":"10.41.0.15","prefix_length":24,
              "ipv6":[{"address":"2001:db8:29::f","prefix_length":64}],
              "advertise":{"mode":"enabled","max_interval_seconds":4,"router_lifetime_seconds":12,
                           "dns":["2001:db8:29::f"]},
              "dhcp_v4_server":{"first":"10.41.0.100","last":"10.41.0.100","router":"10.41.0.15",
                                "dns":["10.41.0.15"],"lease_seconds":3600}}
      }}},
      "routes":[
        {"destination":"198.51.100.0","prefix_length":24,"action":{"kind":"forward","members":[
          {"target":{"kind":"direct","interface":"lan20","gateway":"10.41.0.9"}}]}},
        {"destination":"2001:db8:80::","prefix_length":64,"action":{"kind":"forward","members":[
          {"target":{"kind":"direct","interface":"lan20","gateway":"2001:db8:29::9"}}]}}
      ],
      "policy_routing":{
        "tables":[{"id":100,"routes":[
          {"destination":"203.0.113.0","prefix_length":24,"action":{"kind":"forward","members":[
            {"target":{"kind":"direct","interface":"lan20","gateway":"10.41.0.9"}}]}}
        ]}],
        "rules":[{"priority":10,"source":"0.0.0.0","prefix_length":0,"ingress":"lan10","table":100}]
      },
      "firewall":{"prerouting":[
        {"ingress":"lan10","destination":"198.51.100.7","prefix_length":32,"action":{"kind":"drop"}}
      ]}
    }"#,
    )
    .unwrap();
    let project = |config: &Configuration| {
        let bytes = config
            .bridge_source(|device| {
                Ok([(b"eth0", 2), (b"eth1", 3)]
                    .into_iter()
                    .find_map(|(name, index)| {
                        device.matches(name, [2, 0, 0, 0, 0, 1]).then_some(index)
                    }))
            })
            .unwrap();
        let ports = Ports::parse(&bytes).unwrap();
        crate::inet::project_bridges(&net, ports, Default::default()).unwrap()
    };
    let mut rib = Routes::default();
    let id = rib.reserve_source().unwrap();
    let prepare = |rib: &Routes, config: &Configuration| {
        let projected = project(config);
        let ports = Ports::parse(&projected).unwrap();
        let resolve = |reference: InterfaceReference<'_>| match reference {
            InterfaceReference::Vlan(name) => {
                let index = ports
                    .gateway(name.as_str().as_bytes())
                    .unwrap()
                    .binding
                    .index;
                Ok((index != 0).then_some(index))
            }
            InterfaceReference::Device(_) => unreachable!("fixture references VLAN interfaces"),
        };
        let replacement = [Family::Ipv4, Family::Ipv6].map(|family| {
            let source = config.route_source(family, resolve).unwrap();
            Routes::capture_source(
                id,
                CONFIGURATION_DOMAIN,
                STATIC_PROTOCOL,
                1,
                Batch::parse(&source).unwrap(),
            )
            .unwrap()
        });
        let tables = config.policy_source(resolve).unwrap();
        let input = network_protocol::routing::Configuration::encode(
            &[],
            [None, None],
            None,
            Some(&tables),
        )
        .unwrap();
        let input = network_protocol::routing::Configuration::parse(&input).unwrap();
        let mut facts = routing_facts(&net);
        facts.project_bridges(ports).unwrap();
        let prepared = rib
            .prepare_configuration(&facts, &[], &replacement, None, input.tables())
            .unwrap();
        let firewall = config.firewall_source(resolve).unwrap();
        let batch = network_protocol::ConfigurationBatch::parse(&prepared.encoded)
            .unwrap()
            .with_bridges(ports)
            .with_prerouting(router_protocol::firewall::Prerouting::parse(&firewall).unwrap());
        (
            prepared.next,
            crate::inet::prepare_configuration(&net, Some(batch), None).unwrap(),
        )
    };
    let projected = project(&config);
    let ports = Ports::parse(&projected).unwrap();
    configure_mtu(&net, 3, 1400).unwrap();
    let batch = network_protocol::ConfigurationBatch::default().with_bridges(ports);
    assert!(matches!(
        crate::inet::prepare_configuration(&net, Some(batch), None),
        Err(SocketError::Again)
    ));
    configure_mtu(&net, 3, 1500).unwrap();
    let (rib, transaction) = prepare(&rib, &config);
    assert!(crate::inet::interface_index(&net, b"lan10").is_err());
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    let lane = crate::inet::interface_index(&net, b"lan20").unwrap();
    let snapshot = crate::inet::interface_snapshot(&net, lane).unwrap();
    assert!(snapshot.link.unwrap().carrier);
    assert_eq!(snapshot.address, Some(Ipv4Addr::new(10, 41, 0, 15)));
    let poll = |ms| {
        for _ in 0..5 {
            let progress = stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(Instant::from_millis(ms));
            if !progress.backlog {
                break;
            }
        }
    };
    let drain = |device: &WireDevice| core::mem::take(&mut device.wire.lock().unwrap().1);
    let inject = |device: &WireDevice, bytes| device.wire.lock().unwrap().0.push_back(bytes);
    let tagged = |bytes: &[u8]| {
        let frame = EthernetFrame::parse(bytes).unwrap();
        let header = EthernetHeader {
            source: frame.source(),
            destination: frame.destination(),
            type_or_length: frame.type_or_length(),
            tags: &[VlanTag {
                protocol: CUSTOMER_TAG,
                control: 20,
            }],
        };
        let mut output = vec![0; bytes.len() + 4];
        let at = header.emit(&mut output).unwrap();
        output[at..].copy_from_slice(frame.payload());
        output
    };

    let check_ra = |frames: &[Vec<u8>], lifetime| {
        let mut found = false;
        for bytes in frames {
            let frame = EthernetFrame::parse(bytes).unwrap().network().unwrap();
            if frame.type_or_length() != 0x86dd {
                continue;
            }
            let ip = Ipv6Packet::parse(frame.payload()).unwrap();
            if ip.upper_protocol() != 58 {
                continue;
            }
            let icmp =
                Icmpv6Packet::parse(ip.upper_layer().unwrap(), ip.source(), ip.destination())
                    .unwrap();
            if icmp.message_type() != 134 {
                continue;
            }
            let nd = NdPacket::parse(icmp).unwrap();
            assert_eq!(frame.tags().next().unwrap().id(), 20);
            assert!(ip.source().is_unicast_link_local());
            assert!(
                matches!(nd.message(), NdMessage::RouterAdvertisement { lifetime: seen, .. } if seen == lifetime)
            );
            let mut prefix = false;
            let mut dns = false;
            for option in nd.options().map(|option| option.decode().unwrap()) {
                match option {
                    OptionValue::Prefix {
                        prefix: address,
                        length: 64,
                        valid_lifetime,
                        ..
                    } => {
                        assert_eq!(
                            address,
                            "2001:db8:29::".parse::<core::net::Ipv6Addr>().unwrap()
                        );
                        assert_eq!(valid_lifetime != 0, lifetime != 0);
                        prefix = true;
                    }
                    OptionValue::RecursiveDnsServers {
                        lifetime: dns_lifetime,
                        servers,
                    } => {
                        assert_eq!(dns_lifetime, u32::from(lifetime));
                        assert_eq!(servers, [ipv6_forwarding::B.octets()]);
                        dns = true;
                    }
                    _ => {}
                }
            }
            // Link-local DAD can finish before DAD for the static global address.
            found |= prefix && dns;
        }
        assert!(
            found,
            "expected tagged router advertisement with lifetime {lifetime}, {} other frames",
            frames.len()
        );
    };
    for ms in (0..=8000).step_by(250) {
        poll(ms);
    }
    assert!(
        drain(&access).iter().all(|bytes| {
            let frame = EthernetFrame::parse(bytes).unwrap().network().unwrap();
            frame.type_or_length() != 0x86dd
                || Ipv6Packet::parse(frame.payload())
                    .unwrap()
                    .upper_layer()
                    .is_none_or(|payload| payload.first() != Some(&134))
        }),
        "RA must not leak to VLAN 10"
    );
    check_ra(&drain(&trunk), 12);
    inject(
        &trunk,
        tagged(&peer_arp_frame_for(
            Ipv4Addr::new(10, 41, 0, 15),
            Ipv4Addr::new(10, 41, 0, 9),
        )),
    );
    inject(
        &trunk,
        tagged(&ipv6_forwarding::neighbor(
            NdMessage::NeighborSolicitation {
                target: ipv6_forwarding::B,
            },
            ipv6_forwarding::TO,
            ipv6_forwarding::B,
            ipv6_forwarding::TO_MAC,
        )),
    );
    poll(8001);
    let replies = drain(&trunk);
    assert!(replies.iter().any(|bytes| {
        EthernetFrame::parse(bytes)
            .unwrap()
            .network()
            .unwrap()
            .type_or_length()
            == 0x0806
    }));
    assert!(
        drain(&access).is_empty(),
        "ARP and ND remain within their VLAN"
    );

    trunk
        .transmit_available
        .store(false, core::sync::atomic::Ordering::Relaxed);
    inject(
        &access,
        ipv4_forwarding::datagram(64, 0, &[], b"gateway-v4"),
    );
    inject(
        &access,
        ipv6_forwarding::frame(
            ipv6_forwarding::FROM,
            ipv6_forwarding::TO,
            64,
            253,
            b"gateway-v6",
        ),
    );
    poll(8002);
    assert!(drain(&trunk).is_empty());
    trunk
        .transmit_available
        .store(true, core::sync::atomic::Ordering::Relaxed);
    poll(8012);
    let mut routed = [false; 2];
    for bytes in drain(&trunk) {
        let frame = EthernetFrame::parse(&bytes).unwrap();
        let network = frame.network().unwrap();
        assert_eq!(network.tags().next().unwrap().id(), 20);
        if network.type_or_length() == 0x0800 {
            let ip = Ipv4Packet::parse(network.payload()).unwrap();
            assert_eq!(
                (ip.hop_limit(), ip.payload()),
                (63, b"gateway-v4".as_slice())
            );
            routed[0] = true;
        } else if network.type_or_length() == 0x86dd {
            let ip = Ipv6Packet::parse(network.payload()).unwrap();
            if ip.upper_protocol() == 253 {
                assert_eq!(ip.hop_limit(), 63);
                assert_eq!(ip.upper_layer().unwrap(), b"gateway-v6");
                routed[1] = true;
            }
        }
    }
    assert_eq!(routed, [true, true]);

    let access_index = crate::inet::interface_index(&net, b"lan10").unwrap();
    let query = |destination: &str, ingress| {
        let destination: core::net::IpAddr = destination.parse().unwrap();
        crate::inet::route_lookup(
            &net,
            RouteLookup {
                source: if destination.is_ipv4() {
                    Ipv4Addr::new(10, 40, 0, 9).into()
                } else {
                    ipv6_forwarding::FROM.into()
                },
                destination,
                direction: RouteDirection::Input(ingress),
                mark: 0,
            },
        )
    };
    let main = query("198.51.100.8", access_index).unwrap();
    assert_eq!((main.table, main.interface_index), (254, lane));
    let main6 = query("2001:db8:80::8", access_index).unwrap();
    assert_eq!((main6.table, main6.interface_index), (254, lane));
    let policy = query("203.0.113.8", access_index).unwrap();
    assert_eq!((policy.table, policy.interface_index), (100, lane));
    assert!(query("203.0.113.8", lane).is_err());

    for (last, payload) in [(7, b"blocked".as_slice()), (8, b"allowed".as_slice())] {
        let mut frame = ipv4_forwarding::datagram(64, 0, &[], payload);
        frame[30..34].copy_from_slice(&[198, 51, 100, last]);
        frame[24..26].fill(0);
        let checksum = packet_wire::checksum::checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&checksum.to_be_bytes());
        inject(&access, frame);
    }
    poll(8013);
    let sent = drain(&trunk);
    assert_eq!(sent.len(), 1);
    let frame = EthernetFrame::parse(&sent[0]).unwrap().network().unwrap();
    assert_eq!(frame.tags().next().unwrap().id(), 20);
    assert_eq!(
        Ipv4Packet::parse(frame.payload()).unwrap().payload(),
        b"allowed"
    );
    let hits = crate::inet::firewall_counters(&net, Default::default()).unwrap();
    assert_eq!(hits.rows[0].packets, 1);

    config
        .bridges
        .values_mut()
        .next()
        .unwrap()
        .gateways
        .get_mut(&20)
        .unwrap()
        .advertise = RouterAdvertisements::Disabled;
    let (rib, change) = prepare(&rib, &config);
    change.activate().unwrap();
    change.confirm().unwrap();
    for ms in [11_500, 12_500, 13_500] {
        poll(ms);
    }
    check_ra(&drain(&trunk), 0);
    let pool = config.bridges.values().next().unwrap().gateways[&20]
        .dhcp_pool()
        .unwrap()
        .unwrap();
    exercise_vlan_dhcp(
        &net,
        (access_index, lane),
        (&access, &trunk),
        pool,
        tagged,
        poll,
    );

    config.routes.clear();
    config.policy_routing = Default::default();
    config.firewall = Default::default();
    config
        .bridges
        .values_mut()
        .next()
        .unwrap()
        .gateways
        .remove(&20);
    let (_, removal) = prepare(&rib, &config);
    removal.activate().unwrap();
    assert!(crate::inet::interface_snapshot(&net, lane).is_err());
    removal.abort().unwrap();
    assert_eq!(crate::inet::interface_index(&net, b"lan20").unwrap(), lane);
    assert!(crate::inet::interface_snapshot(&net, lane).unwrap().up);
    let (_, removal) = prepare(&rib, &config);
    removal.activate().unwrap();
    removal.confirm().unwrap();
    assert!(crate::inet::interface_snapshot(&net, lane).is_err());
}

// DHCP 分配算法的单元测试无法检查 VLAN 的真实 packet socket 边界或探测提交。
fn exercise_vlan_dhcp(
    net: &Arc<crate::namespace::Namespace>,
    (access_index, vlan_index): (u32, u32),
    (access, trunk): (&WireDevice, &WireDevice),
    pool: dhcp_server::Pool,
    tagged: impl Fn(&[u8]) -> Vec<u8>,
    poll: impl Fn(i64),
) {
    use core::sync::atomic::Ordering;
    use dhcp_server::{Action, DATAGRAM_BYTES, Reply, Server};
    use network_protocol::{PacketAddress, PacketKind};
    use packet_wire::{
        dhcpv4::{Dhcpv4Builder, Dhcpv4Header, Dhcpv4Packet, MessageType},
        ethernet::{EthernetFrame, EthernetHeader},
        ipv4::{Ipv4Header, Ipv4Packet},
        udp::{self, UdpPacket},
    };
    let target = |index, protocol: u16| PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index as i32,
        hardware_type: 1,
        packet_type: 0,
        address_length: 6,
        address: [255, 255, 255, 255, 255, 255, 0, 0],
    };
    let open = |index, protocol: u16| {
        let socket = crate::packet::PacketSocket::new(
            PacketKind::Datagram,
            protocol.to_be(),
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        socket.bind(target(index, protocol)).unwrap();
        socket
    };
    let other = open(access_index, 0x0800);
    let socket = open(vlan_index, 0x0800);
    let arp = open(vlan_index, 0x0806);
    let client = [2, 0, 0, 0, 0, 9];
    let server_ip = pool.server;
    let dns = pool.options.dns[0];
    let lease = pool.options.first;
    let mut server = Server::new(pool).unwrap();
    let client_frame = |kind, selected| {
        let mut frame = vec![0; 14 + DATAGRAM_BYTES];
        EthernetHeader {
            source: client,
            destination: [255; 6],
            type_or_length: 0x0800,
            tags: &[],
        }
        .emit(&mut frame)
        .unwrap();
        let source = Ipv4Addr::UNSPECIFIED;
        let destination = Ipv4Addr::BROADCAST;
        let mut builder = Dhcpv4Builder::new(
            &mut frame[42..],
            &Dhcpv4Header {
                operation: 1,
                hardware_type: 1,
                hardware_address: &client,
                hops: 0,
                transaction_id: 3,
                seconds: 0,
                broadcast: true,
                client_address: source,
                your_address: source,
                server_address: source,
                relay_address: source,
                server_name: &[],
                boot_file: &[],
            },
        )
        .unwrap();
        builder.option(53, &[kind as u8]).unwrap();
        if selected {
            builder.option(50, &lease.octets()).unwrap();
            builder.option(54, &server_ip.octets()).unwrap();
        }
        let length = 42 + builder.finish().unwrap().len();
        frame.truncate(length);
        udp::emit(&mut frame[34..], source.into(), destination.into(), 68, 67).unwrap();
        Ipv4Header {
            source,
            destination,
            protocol: 17,
            hop_limit: 64,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0x4000,
            options: &[],
        }
        .emit(&mut frame[14..])
        .unwrap();
        tagged(&frame)
    };
    let receive = |kind, selected, time| {
        trunk
            .wire
            .lock()
            .unwrap()
            .0
            .push_back(client_frame(kind, selected));
        poll(time);
        let mut input = crate::receive::ReceiveBuffer::try_new(DATAGRAM_BYTES).unwrap();
        let (size, full, source, _) = socket.receive(&mut input, false).unwrap();
        assert_eq!(
            (size, source.interface_index, &source.address[..6]),
            (full, vlan_index as i32, client.as_slice())
        );
        assert_eq!(
            other.receive(
                &mut crate::receive::ReceiveBuffer::try_new(DATAGRAM_BYTES).unwrap(),
                false
            ),
            Err(SocketError::Again)
        );
        input.into_bytes()
    };

    let mut output = [0; DATAGRAM_BYTES];
    for (time, available) in [(14_000, false), (15_001, true)] {
        let request = receive(MessageType::Discover, false, time as i64);
        let Some(Action::Probe(address)) =
            server.handle(&request, client, time, &mut output).unwrap()
        else {
            panic!("VLAN DHCP must probe the candidate address");
        };
        assert_eq!(address, lease);
        trunk.transmit_available.store(available, Ordering::Relaxed);
        let mut probe = [0; dhcp_server::ARP_BYTES];
        dhcp_server::arp_probe(address, [2, 0, 0, 0, 0, 1], &mut probe).unwrap();
        let result = arp.send_to(&probe, Some(target(vlan_index, 0x0806)));
        server.probe_transmitted(address, available.then_some(time));
        let sent = core::mem::take(&mut trunk.wire.lock().unwrap().1);
        if available {
            assert_eq!(result.unwrap(), dhcp_server::ARP_BYTES);
            let frame = EthernetFrame::parse(&sent[0]).unwrap().network().unwrap();
            assert_eq!(
                (frame.type_or_length(), frame.tags().next().unwrap().id()),
                (0x0806, 20)
            );
        } else {
            assert_eq!(result, Err(SocketError::Again));
            assert!(sent.is_empty());
            assert!(
                server
                    .finish_check(time + 1_000, &mut output)
                    .unwrap()
                    .is_none()
            );
        }
    }
    let send_reply = |reply: Reply<'_>, kind| {
        socket
            .send_to(reply.bytes, Some(target(vlan_index, 0x0800)))
            .unwrap();
        assert!(access.wire.lock().unwrap().1.iter().all(|frame| {
            EthernetFrame::parse(frame)
                .unwrap()
                .network()
                .unwrap()
                .type_or_length()
                != 0x0800
        }));
        let frames = core::mem::take(&mut trunk.wire.lock().unwrap().1);
        let mut ipv4 = frames
            .iter()
            .map(|frame| EthernetFrame::parse(frame).unwrap().network().unwrap())
            .filter(|frame| frame.type_or_length() == 0x0800);
        let wire = ipv4.next().expect("DHCP reply on VLAN 20");
        assert!(ipv4.next().is_none());
        assert_eq!(wire.tags().next().unwrap().id(), 20);
        let ip = Ipv4Packet::parse(wire.payload()).unwrap();
        let udp =
            UdpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into()).unwrap();
        let message = Dhcpv4Packet::parse(udp.payload()).unwrap();
        assert_eq!(message.message_type().unwrap(), kind);
        assert_eq!(message.your_address(), lease);
        assert_eq!(
            message.option(3).unwrap().array::<4>().unwrap(),
            server_ip.octets()
        );
        assert_eq!(
            message.option(6).unwrap().array::<4>().unwrap(),
            dns.octets()
        );
    };
    send_reply(
        server.finish_check(16_001, &mut output).unwrap().unwrap(),
        MessageType::Offer,
    );
    let request = receive(MessageType::Request, true, 16_002);
    let Some(Action::Reply(ack)) = server
        .handle(&request, client, 16_002, &mut output)
        .unwrap()
    else {
        panic!("client selection must obtain a DHCP acknowledgement");
    };
    send_reply(ack, MessageType::Ack);
    arp.close_description();
    socket.close_description();
    other.close_description();
}
