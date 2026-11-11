// 现有 UDP 收发不能证明无人监听、队列拥塞、错误限速及截断引用的分界；直接消费生产 NET/NIC/回环路径。
fn udp_request(source: Ipv6Addr, destination: Ipv6Addr, port: u16, size: usize) -> Vec<u8> {
    let mut frame = vec![0x5a; 62 + size];
    packet_wire::udp::emit(
        &mut frame[54..],
        source.into(),
        destination.into(),
        44000,
        port,
    )
    .unwrap();
    Ipv6Header {
        source,
        destination,
        next_header: 17,
        traffic_class: 0,
        flow_label: 0,
        hop_limit: 64,
    }
    .emit(&mut frame[14..])
    .unwrap();
    EthernetHeader {
        source: MAC,
        destination: if destination.is_multicast() {
            let bytes = destination.octets();
            [0x33, 0x33, bytes[12], bytes[13], bytes[14], bytes[15]]
        } else {
            [2, 0, 0, 0, 0, 1]
        },
        type_or_length: 0x86dd,
        tags: &[],
    }
    .emit(&mut frame)
    .unwrap();
    frame
}

#[test]
fn udp6_closed_port_quotes_exact_prefix_limits_rate_and_never_reflects_invalid_input() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let request = udp_request(PEER, LOCAL, 43320, 1400);
    enqueue(&device, request.clone());
    poll(&net, 2002);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    let (ip, icmp) = parse(&output[0]);
    assert_eq!(
        (
            ip.source(),
            ip.destination(),
            ip.hop_limit(),
            ip.bytes().len()
        ),
        (LOCAL, PEER, 64, 1280)
    );
    assert_eq!((icmp.message_type(), icmp.code()), (1, 4));
    let packet_wire::icmpv6::Message::Error { parameter, body } = icmp.message() else {
        panic!("ICMP error");
    };
    assert_eq!(parameter, 0);
    assert_eq!(body.quoted_bytes().unwrap(), &request[14..1246]);
    assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    endpoint
        .bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 43320, 0, 2,
        )))
        .unwrap();
    // 监听者的八槽满只丢包，不能伪装成无人监听。
    for _ in 0..9 {
        enqueue(&device, udp_request(PEER, LOCAL, 43320, 0));
    }
    poll(&net, 2002);
    assert!(frames(&device).is_empty());
    endpoint.close_description();
    let mut invalid = udp_request(PEER, LOCAL, 43320, 0);
    invalid[60] ^= 1;
    enqueue(&device, invalid);
    for source in [Ipv6Addr::UNSPECIFIED, ALL, Ipv6Addr::LOCALHOST] {
        enqueue(&device, udp_request(source, LOCAL, 43320, 0));
    }
    enqueue(&device, udp_request(PEER, ALL, 43320, 0));
    poll(&net, 2002);
    assert!(frames(&device).is_empty());
    for _ in 0..20 {
        enqueue(&device, udp_request(PEER, LOCAL, 43320, 0));
    }
    poll(&net, 2002);
    assert_eq!(frames(&device).len(), 9);
    enqueue(&device, udp_request(PEER, LOCAL, 43320, 0));
    poll(&net, 2099);
    assert!(frames(&device).is_empty());
    enqueue(&device, udp_request(PEER, LOCAL, 43320, 0));
    poll(&net, 2100);
    assert_eq!(frames(&device).len(), 1);
}

#[test]
fn udp6_loopback_error_is_consumed_once_without_disconnecting_or_releasing_port() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    let peer = "[::1]:43321".parse().unwrap();
    endpoint.connect(peer).unwrap();
    let local = endpoint.address().unwrap();
    for consumer in 0..3 {
        endpoint.send_to(b"missing", None, &[]).unwrap();
        advance_actual_domains();
        assert!(endpoint.poll_state().readable && endpoint.poll_state().error);
        match consumer {
            0 => {
                assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionRefused));
            }
            1 => {
                assert_eq!(
                    udp_read(&endpoint, true),
                    Err(SocketError::ConnectionRefused)
                );
            }
            _ => {
                assert_eq!(
                    endpoint.send_to(b"pending-error", None, &[]),
                    Err(SocketError::ConnectionRefused)
                );
            }
        }
        assert_eq!(endpoint.take_error(), None);
        assert!(!endpoint.poll_state().error);
        assert_eq!(endpoint.address().unwrap(), local);
        assert_eq!(endpoint.peer_address().unwrap(), peer);
    }
    let server = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    server.bind(peer).unwrap();
    endpoint.send_to(b"restored", None, &[]).unwrap();
    advance_actual_domains();
    assert_eq!(udp_read(&server, false).unwrap().0, b"restored");
    server.close_description();
    // 未关联 UDP 不启用 RECVERR 时忽略异步 hard error，不污染后续独立发送。
    endpoint.disconnect().unwrap();
    endpoint.send_to(b"unconnected", Some(peer), &[]).unwrap();
    advance_actual_domains();
    assert_eq!(endpoint.take_error(), None);
    endpoint.close_description();
}

#[test]
fn udp6_received_error_requires_original_tuple_and_accepts_truncated_large_quote() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    endpoint
        .bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 43322, 0, 2,
        )))
        .unwrap();
    endpoint
        .connect(InetAddress::V6(core::net::SocketAddrV6::new(
            PEER, 44000, 0, 2,
        )))
        .unwrap();
    endpoint.send_to(&[0x5a; 1400], None, &[]).unwrap();
    poll(&net, 2002);
    let sent = frames(&device);
    let original = sent
        .iter()
        .find(|frame| Ipv6Packet::parse(&frame[14..]).is_ok_and(|ip| ip.upper_protocol() == 17))
        .unwrap();
    let quote = &original[14..1246];
    let mut error = vec![0; 14 + 40 + 8 + quote.len()];
    packet_wire::icmpv6::emit_error(&mut error[54..], 1, 4, 0, (PEER, LOCAL), quote, &[]).unwrap();
    envelope(&mut error, PEER, LOCAL, false, 64);
    let mut wrong = error.clone();
    // 完整外层 checksum 仍正确，但被引用源端口不属于该 endpoint。
    wrong[102] ^= 1;
    packet_wire::icmpv6::emit(&mut wrong[54..], 1, 4, PEER, LOCAL).unwrap();
    enqueue(&device, wrong);
    poll(&net, 2003);
    assert_eq!(endpoint.take_error(), None);
    enqueue(&device, error);
    poll(&net, 2004);
    assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionRefused));
    assert!(frames(&device).is_empty());
    endpoint.close_description();
}

// PTB 与手工上限必须沿真实准入和 NIC 提交生效；单测 MTU 算法无法保护排队期间的新限制。
#[test]
fn udp6_path_mtu_is_shared_validated_expires_and_rechecks_queued_datagrams() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let socket = || {
        InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap()
    };
    let endpoint = socket();
    assert_eq!(
        endpoint.option(InternetOption::Ipv6Mtu),
        Err(SocketError::NotConnected)
    );
    let remote = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 44000, 0, 2));
    endpoint.connect(remote).unwrap();
    endpoint.send_to(&[0x5a; 1400], None, &[]).unwrap();
    poll(&net, 2002);
    let sent = frames(&device);
    let original = sent
        .iter()
        .find(|frame| Ipv6Packet::parse(&frame[14..]).is_ok_and(|ip| ip.upper_protocol() == 17))
        .unwrap();
    let quote = &original[14..1246];
    let too_big = |mtu: u32, quote: &[u8], code: u8| {
        let mut frame = vec![0; 62 + quote.len()];
        frame[58..62].copy_from_slice(&mtu.to_be_bytes());
        frame[62..].copy_from_slice(quote);
        packet_wire::icmpv6::emit(&mut frame[54..], 2, code, PEER, LOCAL).unwrap();
        envelope(&mut frame, PEER, LOCAL, false, 64);
        frame
    };
    let mut wrong_port = quote.to_vec();
    wrong_port[40] ^= 1;
    for invalid in [
        too_big(1400, &wrong_port, 0),
        too_big(1279, quote, 0),
        too_big(1500, quote, 0),
        too_big(1400, quote, 1),
        too_big(1400, &quote[..47], 0),
    ] {
        enqueue(&device, invalid);
    }
    let mut bad_checksum = too_big(1400, quote, 0);
    bad_checksum[56] ^= 1;
    enqueue(&device, bad_checksum);
    poll(&net, 2003);
    assert_eq!(endpoint.take_error(), None);
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu).unwrap(), 1500);
    endpoint
        .set_ipv6_option(InternetOption::Ipv6DontFragment, 1)
        .unwrap();
    // 该数据报先被旧路径准入；同轮 RX 学习后，TX 必须拒绝它，不能超出新路径继续发出。
    endpoint.send_to(&[0x5a; 1400], None, &[]).unwrap();
    enqueue(&device, too_big(1400, quote, 0));
    poll(&net, 2004);
    assert!(frames(&device).iter().all(|frame| !Ipv6Packet::parse(&frame[14..])
        .is_ok_and(|ip| ip.upper_protocol() == 17)));
    assert_eq!(endpoint.take_error(), Some(SocketError::MessageTooLarge));
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu).unwrap(), 1400);
    assert_eq!(
        endpoint.send_to(&[0x5a; 1353], None, &[]),
        Err(SocketError::MessageTooLarge)
    );
    let second = socket();
    second.connect(remote).unwrap();
    assert_eq!(second.option(InternetOption::Ipv6Mtu).unwrap(), 1400);
    let other = socket();
    let other_address = "fe80::3".parse().unwrap();
    other
        .connect(InetAddress::V6(core::net::SocketAddrV6::new(
            other_address,
            44000,
            0,
            2,
        )))
        .unwrap();
    assert_eq!(other.option(InternetOption::Ipv6Mtu).unwrap(), 1500);
    for invalid in [-1, 1, 1279] {
        assert_eq!(
            endpoint.set_ipv6_option(InternetOption::Ipv6Mtu, invalid),
            Err(SocketError::Invalid)
        );
    }
    endpoint
        .set_ipv6_option(InternetOption::Ipv6Mtu, 1280)
        .unwrap();
    assert_eq!(
        endpoint.send_to(&[0; 1233], None, &[]),
        Err(SocketError::MessageTooLarge)
    );
    // getsockopt 查询路径值，不能把应用上限伪装成网络发现值；0 恢复按路径准入。
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu).unwrap(), 1400);
    endpoint
        .set_ipv6_option(InternetOption::Ipv6Mtu, 0)
        .unwrap();
    endpoint.send_to(&[0; 1352], None, &[]).unwrap();
    poll(&net, 2005);
    let output = frames(&device);
    assert!(output.iter().any(|frame| {
        Ipv6Packet::parse(&frame[14..])
            .is_ok_and(|ip| ip.upper_protocol() == 17 && ip.bytes().len() == 1400)
    }));
    // 更大的 PTB 不得抬高估计；过期判断使用显式单调时刻，不等待十分钟墙钟。
    enqueue(&device, too_big(1420, quote, 0));
    poll(&net, 2006);
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu).unwrap(), 1400);
    endpoint.take_error();
    let path = crate::inet::path_mtu::Path {
        interface: 2,
        source: LOCAL.into(),
        destination: PEER.into(),
    };
    {
        let network = stack(&net).unwrap().lock().unwrap();
        assert_eq!(network.path_mtu.get(path, 1500, 602003), 1400);
        assert_eq!(network.path_mtu.get(path, 1500, 602004), 1500);
        assert_eq!(network.path_mtu.get(path, 1300, 2007), 1300);
    }
    let (isolated, _) = domain();
    assert_eq!(
        stack(&isolated)
            .unwrap()
            .lock()
            .unwrap()
            .path_mtu
            .get(path, 1500, 2007),
        1500
    );
    configure_up(&net, 2, false).unwrap();
    assert_eq!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .path_mtu
            .get(path, 1500, 2007),
        1500
    );
    for endpoint in [endpoint, second, other] {
        endpoint.close_description();
    }
}
