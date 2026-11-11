// IPv4 RAW 无法证明 IPv6 去除扩展头、伪头校验和及作用域；复用同一 NET/NIC 夹具。
fn socket6(net: &Arc<crate::namespace::Namespace>, protocol: u8) -> Arc<InetSocket> {
    InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Raw(protocol),
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap()
}

// 真实提交失败后的回送次数、硬件校验和与 node-local 边界不能由格式测试或代码审查证明。
#[test]
fn ipv6_multicast_udp_output_freezes_policy_and_loops_once_across_link_backpressure() {
    use core::sync::atomic::Ordering;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    let mut nic = WireDevice::new();
    nic.checksum = true;
    let device = Arc::new(nic);
    net.install_protocol(
        prepare_network(
            &net,
            &[device.clone() as Arc<dyn device_model::network::NetworkDevice>],
            true,
        )
        .unwrap(),
    );
    configure_up(&net, 2, true).unwrap();
    ready(&net, &device);
    learn_peer(&net, &device);
    let udp = || {
        InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap()
    };
    let receiver = udp();
    receiver.bind("[::]:44223".parse().unwrap()).unwrap();
    receiver
        .set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    receiver
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
        .unwrap();
    let group: Ipv6Addr = "ff02::5".parse().unwrap();
    receiver.ipv6_membership(group, 2, true).unwrap();
    let sender = udp();
    assert_eq!(sender.option(InternetOption::Ipv6MulticastInterface), Ok(0));
    assert_eq!(sender.option(InternetOption::Ipv6MulticastHopLimit), Ok(1));
    assert_eq!(sender.option(InternetOption::Ipv6MulticastLoop), Ok(1));
    sender
        .set_ipv6_option(InternetOption::Ipv6HopLimit, 77)
        .unwrap();
    assert_eq!(
        sender.set_ipv6_option(InternetOption::Ipv6MulticastHopLimit, 256),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        sender.set_ipv6_option(InternetOption::Ipv6MulticastLoop, 2),
        Err(SocketError::Invalid)
    );
    let target = InetAddress::V6(core::net::SocketAddrV6::new(group, 44223, 0, 2));
    for address in ["ff00::5", "ff0f::5"] {
        assert_eq!(
            sender.send_to(
                b"invalid-scope",
                Some(InetAddress::new(address.parse().unwrap(), 44223)),
                &[]
            ),
            Err(SocketError::Invalid)
        );
    }
    sender.send_to(b"first", Some(target), &[]).unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastHopLimit, 9)
        .unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 0)
        .unwrap();
    poll(&net, 2002);
    poll(&net, 2003);
    let output = frames(&device);
    let packets = output
        .iter()
        .filter_map(|frame| {
            let ethernet = EthernetFrame::parse(frame).unwrap();
            let ip = Ipv6Packet::parse(ethernet.payload()).unwrap();
            (ip.upper_protocol() == 17).then_some((ethernet.destination(), ip))
        })
        .collect::<Vec<_>>();
    assert_eq!(packets.len(), 1);
    let (mac, ip) = packets[0];
    assert_eq!(
        (mac, ip.source(), ip.destination(), ip.hop_limit()),
        ([0x33, 0x33, 0, 0, 0, 5], LOCAL, group, 1)
    );
    assert_eq!(
        packet_wire::udp::UdpPacket::parse(ip.upper_layer().unwrap(), LOCAL.into(), group.into())
            .unwrap()
            .payload(),
        b"first"
    );
    let mut bytes = [0; 64];
    let result = receive(&receiver, &mut bytes, false).unwrap();
    assert_eq!(&bytes[..result.0], b"first");
    assert_eq!(
        result.3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 2,
            destination: group
        })
    );
    assert!(!receiver.poll_state().readable);
    sender.send_to(b"no-loop", Some(target), &[]).unwrap();
    poll(&net, 2004);
    poll(&net, 2005);
    assert!(!receiver.poll_state().readable);
    let output = frames(&device);
    assert!(output.iter().any(|frame| {
        let ip = Ipv6Packet::parse(EthernetFrame::parse(frame).unwrap().payload()).unwrap();
        ip.upper_protocol() == 17 && ip.hop_limit() == 9
    }));
    poll(&net, 4000);
    frames(&device);
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 1)
        .unwrap();
    device.wire.lock().unwrap().2.submit_error =
        Some(device_model::network::NetworkError::WouldBlock);
    sender.send_to(b"retry", Some(target), &[]).unwrap();
    poll(&net, 4001);
    assert!(!receiver.poll_state().readable);
    assert!(frames(&device).is_empty());
    poll(&net, 4001);
    let result = receive(&receiver, &mut bytes, false).unwrap();
    assert_eq!(&bytes[..result.0], b"retry");
    assert!(
        frames(&device).is_empty(),
        "local delivery does not wait for NIC recovery"
    );
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 0)
        .unwrap();
    device.transmit_ready.store(true, Ordering::Relaxed);
    poll(&net, 4002);
    poll(&net, 4003);
    assert_eq!(frames(&device).len(), 1);
    assert!(
        !receiver.poll_state().readable,
        "a failed submit must not duplicate local delivery"
    );
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 1)
        .unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastHopLimit, 0)
        .unwrap();
    device.transmit_available.store(false, Ordering::Relaxed);
    sender.send_to(b"local-only", Some(target), &[]).unwrap();
    poll(&net, 4004);
    poll(&net, 4005);
    let result = receive(&receiver, &mut bytes, false).unwrap();
    assert_eq!(&bytes[..result.0], b"local-only");
    assert!(frames(&device).is_empty());
    let node: Ipv6Addr = "ff01::5".parse().unwrap();
    receiver.ipv6_membership(node, 2, true).unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastHopLimit, 100)
        .unwrap();
    sender
        .send_to(
            b"interface-only",
            Some(InetAddress::V6(core::net::SocketAddrV6::new(
                node, 44223, 0, 2,
            ))),
            &[],
        )
        .unwrap();
    poll(&net, 4006);
    poll(&net, 4007);
    let result = receive(&receiver, &mut bytes, false).unwrap();
    assert_eq!(&bytes[..result.0], b"interface-only");
    assert!(frames(&device).is_empty());
    let mut invalid = vec![0; 66];
    envelope(&mut invalid, PEER, node, false, 1);
    invalid[20] = 17;
    packet_wire::udp::emit(&mut invalid[54..], PEER.into(), node.into(), 44000, 44223).unwrap();
    enqueue(&device, invalid);
    poll(&net, 4007);
    assert!(
        !receiver.poll_state().readable,
        "interface-local groups cannot arrive from an Ethernet link"
    );
    device.transmit_available.store(true, Ordering::Relaxed);
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastHopLimit, -1)
        .unwrap();
    assert_eq!(sender.option(InternetOption::Ipv6MulticastHopLimit), Ok(1));
    receiver.ipv6_membership(group, 1, true).unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastInterface, 1)
        .unwrap();
    sender
        .send_to(
            b"lo-device",
            Some(InetAddress::new(group.into(), 44223)),
            &[],
        )
        .unwrap();
    poll(&net, 4008);
    poll(&net, 4009);
    let result = receive(&receiver, &mut bytes, false).unwrap();
    assert_eq!(&bytes[..result.0], b"lo-device");
    assert_eq!(
        result.3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 1,
            destination: group
        })
    );
    assert!(!receiver.poll_state().readable);
    sender
        .send_to(
            b"unicast",
            Some(InetAddress::V6(core::net::SocketAddrV6::new(
                PEER, 44223, 0, 2,
            ))),
            &[],
        )
        .unwrap();
    poll(&net, 4010);
    assert!(frames(&device).iter().any(|frame| {
        let ip = Ipv6Packet::parse(EthernetFrame::parse(frame).unwrap().payload()).unwrap();
        ip.upper_protocol() == 17 && ip.destination() == PEER && ip.hop_limit() == 77
    }));
    sender.close_description();
    receiver.close_description();
}

#[test]
fn ipv6_multicast_raw_output_honors_interface_precedence_checksum_and_frozen_retry() {
    use core::sync::atomic::Ordering;
    use network_protocol::InternetSendPacketInfo;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    let first = Arc::new(WireDevice::new());
    let second = Arc::new(WireDevice::new());
    let devices = [
        first.clone() as Arc<dyn device_model::network::NetworkDevice>,
        second.clone(),
    ];
    net.install_protocol(prepare_network(&net, &devices, true).unwrap());
    configure_up(&net, 2, true).unwrap();
    configure_up(&net, 3, true).unwrap();
    ready(&net, &first);
    frames(&second);
    let group: Ipv6Addr = "ff02::5".parse().unwrap();
    let receiver = socket6(&net, 89);
    receiver
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
        .unwrap();
    receiver
        .set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    receiver
        .set_ipv6_option(InternetOption::Ipv6Checksum, 12)
        .unwrap();
    receiver.ipv6_membership(group, 2, true).unwrap();
    receiver.ipv6_membership(group, 3, true).unwrap();
    poll(&net, 2002);
    poll(&net, 4000);
    frames(&first);
    frames(&second);
    let sender = socket6(&net, 89);
    sender
        .set_ipv6_option(InternetOption::Ipv6Checksum, 12)
        .unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastInterface, 3)
        .unwrap();
    assert_eq!(
        sender.set_ipv6_option(InternetOption::Ipv6MulticastInterface, 999),
        Err(SocketError::NoDevice)
    );
    assert_eq!(sender.option(InternetOption::Ipv6MulticastInterface), Ok(3));
    let target = InetAddress::new(group.into(), 0);
    let payload = [7; 16];
    sender
        .send_to(
            &payload,
            Some(target),
            &[InternetSendPacketInfo::Ipv6 {
                interface_index: Some(2),
                source_address: LOCAL,
            }],
        )
        .unwrap();
    poll(&net, 4001);
    poll(&net, 4002);
    let output = frames(&first);
    assert_eq!(output.len(), 1);
    assert!(frames(&second).is_empty());
    let ip = Ipv6Packet::parse(EthernetFrame::parse(&output[0]).unwrap().payload()).unwrap();
    assert_eq!(
        (
            ip.source(),
            ip.destination(),
            ip.upper_protocol(),
            ip.hop_limit()
        ),
        (LOCAL, group, 89, 1)
    );
    assert_eq!(
        packet_wire::checksum::transport_checksum(
            LOCAL.into(),
            group.into(),
            89,
            ip.upper_layer().unwrap()
        ),
        Ok(0)
    );
    let mut bytes = [0; 64];
    assert_eq!(
        receive(&receiver, &mut bytes, false).unwrap().3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 2,
            destination: group
        })
    );
    assert!(
        !sender.poll_state().readable,
        "MulticastAll=0 excludes a non-member sender's own copy"
    );
    let scoped = InetAddress::V6(core::net::SocketAddrV6::new(group, 0, 0, 2));
    sender.send_to(&payload, Some(scoped), &[]).unwrap();
    poll(&net, 4003);
    poll(&net, 4004);
    assert_eq!(frames(&first).len(), 1);
    assert!(frames(&second).is_empty());
    receive(&receiver, &mut bytes, false).unwrap();
    sender.bind_to_device(b"eth1", false).unwrap();
    assert_eq!(
        sender.set_ipv6_option(InternetOption::Ipv6MulticastInterface, 2),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        sender.send_to(&payload, Some(scoped), &[]),
        Err(SocketError::Invalid)
    );
    sender.send_to(&payload, Some(target), &[]).unwrap();
    poll(&net, 4005);
    poll(&net, 4006);
    assert!(frames(&first).is_empty());
    assert_eq!(frames(&second).len(), 1);
    assert_eq!(
        receive(&receiver, &mut bytes, false).unwrap().3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 3,
            destination: group
        })
    );
    sender.bind_to_device(b"", true).unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastInterface, 0)
        .unwrap();
    first.wire.lock().unwrap().2.submit_error =
        Some(device_model::network::NetworkError::WouldBlock);
    sender.send_to(&payload, Some(target), &[]).unwrap();
    poll(&net, 4007);
    assert!(frames(&first).is_empty());
    assert!(!receiver.poll_state().readable);
    poll(&net, 4007);
    assert_eq!(
        receive(&receiver, &mut bytes, false).unwrap().3,
        Some(InternetPacketInfo::Ipv6 {
            interface_index: 2,
            destination: group
        })
    );
    assert!(frames(&first).is_empty());
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 0)
        .unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastInterface, 3)
        .unwrap();
    first.transmit_ready.store(true, Ordering::Relaxed);
    poll(&net, 4008);
    poll(&net, 4009);
    assert_eq!(frames(&first).len(), 1);
    assert!(frames(&second).is_empty());
    assert!(!receiver.poll_state().readable);
    sender.set_header_included(true).unwrap();
    sender
        .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 1)
        .unwrap();
    let mut included = vec![0; 56];
    included[40..].copy_from_slice(&payload);
    Ipv6Header {
        source: LOCAL,
        destination: group,
        next_header: 89,
        traffic_class: 0,
        flow_label: 0,
        hop_limit: 0,
    }
    .emit(&mut included)
    .unwrap();
    included[52..54].fill(0);
    let checksum =
        packet_wire::checksum::transport_checksum(LOCAL.into(), group.into(), 89, &included[40..])
            .unwrap();
    included[52..54].copy_from_slice(&checksum.to_be_bytes());
    sender.send_to(&included, Some(target), &[]).unwrap();
    poll(&net, 4010);
    poll(&net, 4011);
    assert!(frames(&first).is_empty() && frames(&second).is_empty());
    assert_eq!(
        receive(&receiver, &mut bytes, false).unwrap().0,
        payload.len()
    );
    assert_eq!(&bytes[..payload.len()], &included[40..]);
    sender.close_description();
    receiver.close_description();
}

// 共享 MLD 引用、退出即时生效与 OOM 原子性无法用单播收发或静态检查证明；复用实际 NET/NIC。
#[test]
fn ipv6_multicast_memberships_filter_udp_raw_and_retire_only_the_last_reference() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let group: Ipv6Addr = "ff02::5".parse().unwrap();
    let raw = socket6(&net, 17);
    raw.set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    let udp = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    udp.set_reuse_address(true).unwrap();
    udp.set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    udp.bind(InetAddress::V6(core::net::SocketAddrV6::new(
        group, 44222, 0, 2,
    )))
    .unwrap();
    let observer = socket6(&net, 17);
    let excluded = socket6(&net, 17);
    excluded
        .set_ipv6_option(InternetOption::Ipv6MulticastAll, 0)
        .unwrap();
    let unicast = socket6(&net, 17);
    unicast
        .bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 0, 0, 2,
        )))
        .unwrap();
    let mut frame = vec![0x5a; 66];
    envelope(&mut frame, PEER, group, false, 1);
    frame[20] = 17;
    packet_wire::udp::emit(&mut frame[54..], PEER.into(), group.into(), 44000, 44222).unwrap();
    enqueue(&device, frame.clone());
    poll(&net, 2002);
    assert!(
        !observer.poll_state().readable,
        "unjoined IP group is rejected before socket demux"
    );
    crate::allocation_failure::fail_next();
    assert_eq!(
        raw.ipv6_membership(group, 2, true),
        Err(SocketError::NoMemory)
    );
    assert!(crate::allocation_failure::was_consumed());
    raw.ipv6_membership(group, 2, true).unwrap();
    assert_eq!(
        raw.ipv6_membership(group, 0, true),
        Err(SocketError::AddressInUse)
    );
    assert_eq!(
        raw.ipv6_membership(PEER, 2, true),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        raw.ipv6_membership(group, 999, true),
        Err(SocketError::NoDevice)
    );
    udp.ipv6_membership(group, 0, true).unwrap();
    enqueue(&device, frame.clone());
    poll(&net, 2003);
    let mut bytes = [0; 64];
    assert_eq!(receive(&raw, &mut bytes, false).unwrap().0, 12);
    assert_eq!(receive(&observer, &mut bytes, false).unwrap().0, 12);
    assert_eq!(receive(&udp, &mut bytes, false).unwrap().0, 4);
    assert_eq!(&bytes[..4], &[0x5a; 4]);
    assert!(!excluded.poll_state().readable);
    assert!(!unicast.poll_state().readable);
    raw.ipv6_membership(group, 0, false).unwrap();
    assert_eq!(
        raw.ipv6_membership(group, 2, false),
        Err(SocketError::AddressNotAvailable)
    );
    enqueue(&device, frame.clone());
    poll(&net, 2004);
    assert!(!raw.poll_state().readable);
    assert!(
        udp.poll_state().readable,
        "another member keeps the interface joined"
    );
    receive(&udp, &mut bytes, false).unwrap();
    receive(&observer, &mut bytes, false).unwrap();
    udp.close_description();
    enqueue(&device, frame.clone());
    poll(&net, 2005);
    assert!(
        !observer.poll_state().readable,
        "leave retransmission is not receive membership"
    );
    raw.ipv6_membership(group, 2, true).unwrap();
    configure_up(&net, 2, false).unwrap();
    poll(&net, 3000);
    configure_up(&net, 2, true).unwrap();
    for ms in [4000, 5000, 6000, 6001] {
        poll(&net, ms);
    }
    enqueue(&device, frame);
    poll(&net, 6002);
    assert!(
        raw.poll_state().readable,
        "interface restart preserves socket references and rejoins"
    );
    for socket in [&raw, &observer, &excluded, &unicast] {
        socket.close_description();
    }
}

#[test]
fn ipv6_multicast_new_groups_share_interface_query_compatibility_and_skip_all_nodes_reports() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    enqueue(&device, query(GROUP, &[], true));
    poll(&net, 2002);
    frames(&device);
    let group: Ipv6Addr = "ff02::5".parse().unwrap();
    let raw = socket6(&net, 89);
    raw.ipv6_membership(group, 2, true).unwrap();
    raw.ipv6_membership(ALL, 2, true).unwrap();
    poll(&net, 2003);
    let output = frames(&device);
    assert!(output.iter().any(|frame| {
        let (_, icmp) = parse(frame);
        matches!(MldMessage::parse(icmp), Ok(MldMessage::ReportV1 { done: false, group: address }) if address == group)
    }), "new group must inherit the interface's active MLDv1 querier");
    assert!(!output.iter().any(|frame| {
        let (_, icmp) = parse(frame);
        matches!(
            MldMessage::parse(icmp),
            Ok(MldMessage::ReportV1 { group: ALL, .. })
        )
    }));
    raw.ipv6_membership(GROUP, 2, true).unwrap();
    raw.ipv6_membership(GROUP, 2, false).unwrap();
    raw.close_description();
    poll(&net, 2004);
    assert!(
        !frames(&device).iter().any(|frame| {
            let (_, icmp) = parse(frame);
            matches!(
                MldMessage::parse(icmp),
                Ok(MldMessage::ReportV1 {
                    done: true,
                    group: GROUP
                })
            )
        }),
        "solicited-node group still belongs to the address"
    );
    let loopback = socket6(&net, 89);
    loopback.ipv6_membership(group, 1, true).unwrap();
    poll(&net, 2005);
    assert!(
        !stack(&net).unwrap().lock().unwrap().interfaces[0]
            .ipv6
            .as_ref()
            .unwrap()
            .due(2005),
        "loopback membership must not retain an unsendable MLD deadline"
    );
    loopback.close_description();
}
#[test]
fn raw_connected_loopback_filters_and_metadata_cover_both_families() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let four = scoped_endpoint(&net, InetProtocol::Raw(253));
    four.connect("127.0.0.1:0".parse().unwrap()).unwrap();
    assert_eq!(four.address().unwrap().ip(), Ipv4Addr::LOCALHOST);
    four.send_to(b"native-four", None, &[]).unwrap();
    advance_actual_domains();
    four.disconnect().unwrap();
    let mut four_bytes = [0; 64];
    let received = receive(&four, &mut four_bytes, false).unwrap();
    assert_eq!(&four_bytes[20..received.0], b"native-four");
    assert_eq!(four.address().unwrap().ip(), Ipv4Addr::UNSPECIFIED);
    assert_eq!(
        four.send_to(b"missing-default", None, &[]),
        Err(SocketError::DestinationRequired)
    );
    four.close_description();
    let socket = socket6(&net, 58);
    socket
        .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
        .unwrap();
    assert_eq!(socket.option(InternetOption::Ipv6Checksum), Ok(2));
    let request = [128, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    socket
        .send_to(&request, Some("[::1]:0".parse().unwrap()), &[])
        .unwrap();
    advance_actual_domains();
    let mut bytes = [0; 64];
    let mut reply = false;
    while socket.poll_state().readable {
        let peeked = receive(&socket, &mut bytes[..3], true).unwrap();
        assert_eq!((peeked.0, peeked.1), (3, request.len()));
        let result = receive(&socket, &mut bytes, false).unwrap();
        assert_eq!(result.1, request.len());
        assert_eq!(result.2, Some("[::1]:0".parse().unwrap()));
        assert!(matches!(
            result.3,
            Some(InternetPacketInfo::Ipv6 {
                interface_index: 1,
                destination: Ipv6Addr::LOCALHOST
            })
        ));
        assert_eq!(
            packet_wire::checksum::transport_checksum(
                Ipv6Addr::LOCALHOST.into(),
                Ipv6Addr::LOCALHOST.into(),
                58,
                &bytes[..result.0]
            ),
            Ok(0)
        );
        if bytes[0] == 129 {
            reply = true;
            assert_eq!(&bytes[4..result.0], &request[4..]);
        }
    }
    assert!(
        reply,
        "RAW must observe the actual ICMPv6 reply without its IPv6 header"
    );
    socket.connect("[::1]:0".parse().unwrap()).unwrap();
    let mut filter = [u32::MAX; 8];
    filter[4] &= !(1 << 1);
    socket
        .icmp_filter(Some(network_protocol::IcmpFilterUpdate {
            mask: [u32::MAX; 8],
            blocked: filter,
        }))
        .unwrap();
    socket.send_to(&request, None, &[]).unwrap();
    advance_actual_domains();
    // 新策略只影响新报文；已经放行的 Echo Reply 不随更新被丢弃。
    socket
        .icmp_filter(Some(network_protocol::IcmpFilterUpdate {
            mask: [u32::MAX; 8],
            blocked: [u32::MAX; 8],
        }))
        .unwrap();
    let received = receive(&socket, &mut bytes, false).unwrap();
    assert_eq!(bytes[0], 129);
    assert_eq!(received.0, request.len());
    assert!(
        !socket.poll_state().readable,
        "request type must be filtered independently of builtin Echo processing"
    );
    let mut patch = network_protocol::IcmpFilterUpdate::default();
    patch.mask[4] = 2;
    socket.icmp_filter(Some(patch)).unwrap();
    assert_eq!(
        socket.icmp_filter(None).unwrap(),
        filter,
        "masked update must preserve every unselected type"
    );
    socket.send_to(&request, None, &[]).unwrap();
    advance_actual_domains();
    socket.disconnect().unwrap();
    assert_eq!(socket.peer_address(), Err(SocketError::NotConnected));
    let received = receive(&socket, &mut bytes, false).unwrap();
    assert_eq!(received.2, Some("[::1]:0".parse().unwrap()));
    assert_eq!(bytes[0], 129);
    socket.connect("[::1]:0".parse().unwrap()).unwrap();
    socket.shutdown(1).unwrap();
    assert_eq!(
        socket.send_to(&request, None, &[]),
        Err(SocketError::BrokenPipe)
    );
    socket.shutdown(0).unwrap();
    assert_eq!(
        receive(&socket, &mut bytes, false).unwrap(),
        (0, 0, None, None)
    );
    assert!(socket.poll_state().hangup);
    let unconnected = socket6(&net, 58);
    assert_eq!(unconnected.shutdown(0), Err(SocketError::NotConnected));
    assert!(unconnected.poll_state().readable && unconnected.poll_state().read_closed);
    assert_eq!(
        receive(&unconnected, &mut bytes, false).unwrap(),
        (0, 0, None, None)
    );
    unconnected.close_description();
    socket.close_description();
    advance_actual_domains();
}
#[test]
fn raw6_actual_nic_checks_checksum_and_protocol_then_encodes_selected_source() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    let socket = socket6(&net, 89);
    socket.bind_to_device(b"eth0", false).unwrap();
    socket
        .set_ipv6_option(InternetOption::Ipv6Checksum, 12)
        .unwrap();
    assert_eq!(
        socket.set_ipv6_option(InternetOption::Ipv6Checksum, 3),
        Err(SocketError::Invalid)
    );
    let mut frame = vec![0; 54 + 24];
    frame[54..].fill(7);
    frame[66..68].fill(0);
    let checksum =
        packet_wire::checksum::transport_checksum(PEER.into(), LOCAL.into(), 89, &frame[54..])
            .unwrap();
    frame[66..68].copy_from_slice(&checksum.to_be_bytes());
    envelope(&mut frame, PEER, LOCAL, false, 64);
    frame[20] = 89;
    let mut invalid = frame.clone();
    invalid[77] ^= 1;
    enqueue(&device, invalid);
    poll(&net, 2002);
    assert!(
        !socket.poll_state().readable,
        "configured RAW checksum rejects corruption"
    );
    let accepted = frame.clone();
    enqueue(&device, frame);
    poll(&net, 2003);
    let mut bytes = [0; 32];
    let result = receive(&socket, &mut bytes, false).unwrap();
    assert_eq!(result.0, 24);
    assert_eq!(
        result.2,
        Some(InetAddress::V6(core::net::SocketAddrV6::new(PEER, 0, 0, 2)))
    );
    let target = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 0, 0, 2));
    socket.connect(target).unwrap();
    assert_eq!(socket.peer_address(), Ok(target));
    assert_eq!(
        socket.connect("[2001:db8:ffff::1]:0".parse().unwrap()),
        Err(SocketError::NetworkUnreachable)
    );
    assert_eq!(
        socket.peer_address(),
        Ok(target),
        "failed reconnect must keep the original peer"
    );
    assert_eq!(
        socket.icmp_filter(None),
        Err(SocketError::OperationNotSupported)
    );
    socket.send_to(&[9; 24], None, &[]).unwrap();
    poll(&net, 2004);
    let output = frames(&device);
    let ip = output
        .iter()
        .find_map(|frame| {
            let ip = Ipv6Packet::parse(&frame[14..]).ok()?;
            (ip.upper_protocol() == 89).then_some(ip)
        })
        .expect("actual RAW output");
    assert_eq!((ip.source(), ip.destination()), (LOCAL, PEER));
    assert_eq!(
        packet_wire::checksum::transport_checksum(
            LOCAL.into(),
            PEER.into(),
            89,
            ip.upper_layer().unwrap()
        ),
        Ok(0)
    );
    let mut other = accepted.clone();
    let other_source: Ipv6Addr = "fe80::44".parse().unwrap();
    other[22..38].copy_from_slice(&other_source.octets());
    other[66..68].fill(0);
    let sum = packet_wire::checksum::transport_checksum(
        other_source.into(),
        LOCAL.into(),
        89,
        &other[54..],
    )
    .unwrap();
    other[66..68].copy_from_slice(&sum.to_be_bytes());
    enqueue(&device, other);
    poll(&net, 2005);
    assert!(
        !socket.poll_state().readable,
        "connected RAW excludes another source before enqueue"
    );
    enqueue(&device, accepted);
    poll(&net, 2006);
    socket.disconnect().unwrap();
    let received = receive(&socket, &mut bytes, false).unwrap();
    assert_eq!(received.0, 24);
    assert_eq!(
        received.2,
        Some(target),
        "disconnect preserves the packet's original scope"
    );
    socket.bind_to_device(b"lo", false).unwrap();
    socket
        .bind(InetAddress::V6(core::net::SocketAddrV6::new(
            LOCAL, 0, 0, 2,
        )))
        .unwrap();
    assert_eq!(
        socket.option(InternetOption::BoundToDevice),
        Ok(2),
        "explicit RAW bind zone replaces the previous device"
    );
    socket.connect(target).unwrap();
    socket.disconnect().unwrap();
    assert_eq!(
        socket.address().unwrap().ip(),
        Ipv6Addr::UNSPECIFIED,
        "RAW disconnect clears source binding; it has no UDP bind-address lock"
    );
    let unresolved = InetAddress::V6(core::net::SocketAddrV6::new(
        "fe80::33".parse().unwrap(),
        0,
        0,
        2,
    ));
    for _ in 0..8 {
        socket.send_to(&[9; 24], Some(unresolved), &[]).unwrap();
    }
    assert!(!socket.poll_state().writable);
    for ms in [2010, 2011, 3011, 4011, 5011] {
        poll(&net, ms);
    }
    assert!(
        socket.poll_state().writable,
        "failed neighbor must release RAW queue capacity"
    );
    socket.close_description();
    poll(&net, 5012);
    assert!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .raw_endpoints
            .is_empty()
    );
}
