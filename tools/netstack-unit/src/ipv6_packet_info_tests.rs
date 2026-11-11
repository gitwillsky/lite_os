// 冻结 ingress 与实时选项、按 wire 族有序执行控制项，均不能由 IPv4 单次覆盖用例证明。
use network_protocol::{InetFamily, InternetPacketInfo, InternetSendPacketInfo as Control};

#[test]
fn udp6_packet_info_selects_one_send_and_receives_frozen_facts_with_live_options() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let endpoint = InetSocket::new(
        InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    endpoint.bind("[::]:43310".parse().unwrap()).unwrap();
    let peer = InetAddress::new(PEER.into(), 44000);
    let control = Control::Ipv6 {
        interface_index: Some(2),
        source_address: LOCAL,
    };
    let unchanged = endpoint.address().unwrap();
    endpoint
        .send_to(
            b"control",
            Some(peer),
            &[
                control,
                Control::Ipv6 {
                    interface_index: None,
                    source_address: Ipv6Addr::UNSPECIFIED,
                },
            ],
        )
        .unwrap();
    assert_eq!(endpoint.address().unwrap(), unchanged);
    assert_eq!(endpoint.option(InternetOption::BoundToDevice), Ok(0));
    assert_eq!(
        endpoint.send_to(b"no-zone", Some(peer), &[]),
        Err(SocketError::Invalid)
    );
    for (controls, error) in [
        (
            vec![Control::Invalid(InetFamily::Ipv6), control],
            SocketError::Invalid,
        ),
        (
            vec![
                Control::Ipv6 {
                    interface_index: Some(2),
                    source_address: PEER,
                },
                control,
            ],
            SocketError::Invalid,
        ),
        (
            vec![Control::Ipv6 {
                interface_index: Some(99),
                source_address: Ipv6Addr::UNSPECIFIED,
            }],
            SocketError::NoDevice,
        ),
        (
            vec![Control::Ipv6 {
                interface_index: None,
                source_address: LOCAL,
            }],
            SocketError::Invalid,
        ),
        (
            vec![
                control,
                Control::Ipv6 {
                    interface_index: Some(1),
                    source_address: Ipv6Addr::UNSPECIFIED,
                },
            ],
            SocketError::Invalid,
        ),
    ] {
        assert_eq!(
            endpoint.send_to(b"invalid", Some(peer), &controls),
            Err(error)
        );
    }
    // IPv4 控制项不参与 native IPv6 发送，即使对应 IPv4 数据宽度无效。
    endpoint
        .send_to(
            b"ignored-v4",
            Some(peer),
            &[Control::Invalid(InetFamily::Ipv4), control],
        )
        .unwrap();
    poll(&net, 2002);
    let output = frames(&device);
    let datagrams: Vec<_> = output
        .iter()
        .filter_map(|frame| {
            let ip = Ipv6Packet::parse(&frame[14..]).ok()?;
            (ip.upper_protocol() == 17).then_some(ip)
        })
        .collect();
    assert_eq!(datagrams.len(), 2);
    assert!(
        datagrams
            .iter()
            .all(|ip| ip.source() == LOCAL && ip.destination() == PEER)
    );
    let mut incoming = vec![0; 65];
    EthernetHeader {
        source: MAC,
        destination: [2, 0, 0, 0, 0, 1],
        type_or_length: 0x86dd,
        tags: &[],
    }
    .emit(&mut incoming)
    .unwrap();
    Ipv6Header {
        source: PEER,
        destination: LOCAL,
        next_header: 17,
        traffic_class: 0,
        flow_label: 0,
        hop_limit: 64,
    }
    .emit(&mut incoming[14..])
    .unwrap();
    packet_wire::udp::UdpHeader {
        source: PEER.into(),
        destination: LOCAL.into(),
        source_port: 44000,
        destination_port: 43310,
    }
    .emit_prefix(&mut incoming[54..62], b"udp", true)
    .unwrap();
    incoming[62..].copy_from_slice(b"udp");
    enqueue(&device, incoming);
    poll(&net, 2003);
    let mut bytes = [0; 8];
    assert_eq!(receive(&endpoint, &mut bytes, true).unwrap().3, None);
    endpoint
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
        .unwrap();
    let expected = Some(InternetPacketInfo::Ipv6 {
        interface_index: 2,
        destination: LOCAL,
    });
    assert_eq!(receive(&endpoint, &mut bytes, true).unwrap().3, expected);
    configure_up(&net, 2, false).unwrap();
    assert_eq!(receive(&endpoint, &mut bytes, false).unwrap().3, expected);
    assert_eq!(&bytes[..3], b"udp");
    endpoint.close_description();
}

#[test]
fn mapped_udp_selects_requested_metadata_and_folds_controls_in_user_order() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let server = InetSocket::new(
        InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    server.bind("[::]:43311".parse().unwrap()).unwrap();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    let target = "127.0.0.1:43311".parse().unwrap();
    // AF_INET 忽略 SOL_IPV6；实际 IPv4 控制项仍生效。
    sender
        .send_to(
            b"mapped",
            Some(target),
            &[Control::Invalid(InetFamily::Ipv6)],
        )
        .unwrap();
    advance_actual_domains();
    let mut bytes = [0; 8];
    assert_eq!(receive(&server, &mut bytes, true).unwrap().3, None);
    server
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
        .unwrap();
    assert_eq!(
        receive(&server, &mut bytes, true).unwrap().3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 1,
            destination: Ipv4Addr::LOCALHOST.to_ipv6_mapped()
        })
    );
    server.set_packet_info(true).unwrap();
    assert_eq!(
        receive(&server, &mut bytes, true).unwrap().3,
        Some(InternetPacketInfo::DualStack {
            interface_index: 1,
            specific_address: Ipv4Addr::LOCALHOST,
            destination: Ipv4Addr::LOCALHOST
        })
    );
    server
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 0)
        .unwrap();
    assert_eq!(
        receive(&server, &mut bytes, false).unwrap().3,
        Some(InternetPacketInfo::Ipv4 {
            interface_index: 1,
            specific_address: Ipv4Addr::LOCALHOST,
            destination: Ipv4Addr::LOCALHOST
        })
    );
    let invalid = Control::Ipv6 {
        interface_index: None,
        source_address: Ipv6Addr::UNSPECIFIED,
    };
    assert_eq!(
        server.send_to(b"bad", Some(target), &[invalid]),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        server.send_to(b"bad", Some(target), &[Control::Invalid(InetFamily::Ipv4)]),
        Err(SocketError::Invalid)
    );
    // IPv4 最后一个显式源胜出，零地址清除该覆盖；中途的非本地源不提前拒绝。
    server
        .send_to(
            b"ordered",
            Some(target),
            &[
                Control::Ipv4 {
                    interface_index: Some(1),
                    source_address: Ipv4Addr::new(192, 0, 2, 1),
                },
                Control::Ipv6 {
                    interface_index: None,
                    source_address: Ipv4Addr::UNSPECIFIED.to_ipv6_mapped(),
                },
            ],
        )
        .unwrap();
    advance_actual_domains();
    assert_eq!(udp_read(&server, false).unwrap().0, b"ordered");
    sender.close_description();
    server.close_description();
}
