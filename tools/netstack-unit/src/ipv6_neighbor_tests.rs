// DAD/组播与 Echo 格式断言无法覆盖异步邻居的学习、NUD、排队、背压和 OOM 回滚。
// 这里只编码链路对端的标准报文；缓存、索引、时钟和载荷生命周期执行同一生产 owner。
fn advertisement(target: Ipv6Addr, mac: Option<[u8; 6]>, flags: u32) -> Vec<u8> {
    let mut frame = vec![0; 78 + usize::from(mac.is_some()) * 8];
    if let Some(mac) = mac {
        OptionValue::LinkLayerAddress {
            source: false,
            address: &mac,
        }
        .emit(&mut frame[78..])
        .unwrap();
    }
    NdMessage::NeighborAdvertisement { flags, target }
        .emit(&mut frame[54..], target, LOCAL)
        .unwrap();
    envelope(&mut frame, target, LOCAL, false, 255);
    // IPv6 邻居信息来自 TLLA；帧的发送方可能是代理，不能把它作为缓存更新依据。
    frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 99]);
    frame
}
fn assert_probe(frame: &[u8], destination: Ipv6Addr, destination_mac: [u8; 6]) {
    let ethernet = EthernetFrame::parse(frame).unwrap();
    assert_eq!(ethernet.destination(), destination_mac);
    let (ip, icmp) = parse(frame);
    assert_eq!(
        (ip.source(), ip.destination(), ip.hop_limit()),
        (LOCAL, destination, 255)
    );
    let packet = NdPacket::parse(icmp).unwrap();
    assert!(matches!(
        packet.message(),
        NdMessage::NeighborSolicitation { target: PEER }
    ));
    let option = packet.options().next().unwrap();
    assert_eq!(option.kind(), 1);
    assert_eq!(&option.bytes()[2..], &[2, 0, 0, 0, 0, 1]);
}

// 默认 NUD 时序用例无法证明通告参数进入真实计时器，或零值保留已采用的参数。
#[test]
fn router_advertisement_controls_ipv6_output_and_neighbor_timers() {
    use network_protocol::{Forwarding, InternetOption, NeighborState};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let device_mtu = interface_snapshot(&net, 2).unwrap().mtu;
    let configuration = prepare_configuration(
        &net,
        core::iter::empty(),
        Some(Forwarding { ipv4: true, ipv6: false }),
    ).unwrap();
    configuration.activate().unwrap();
    configuration.confirm().unwrap();

    let advertise = |reachable_time, retransmit_timer, hops, hop_limit, mtu: Option<u32>| {
        let mut frame = vec![0; 70 + usize::from(mtu.is_some()) * 8];
        if let Some(mtu) = mtu {
            OptionValue::Mtu(mtu).emit(&mut frame[70..]).unwrap();
        }
        NdMessage::RouterAdvertisement {
            hop_limit,
            flags: 0,
            lifetime: 0,
            reachable_time,
            retransmit_timer,
        }.emit(&mut frame[54..], PEER, ALL).unwrap();
        envelope(&mut frame, PEER, ALL, false, hops);
        enqueue(&device, frame);
    };
    advertise(200, 240, 255, 37, Some(1280));
    advertise(0, 0, 255, 0, Some(1200));
    advertise(0, 0, 255, 0, Some(device_mtu as u32 + 1));
    advertise(3000, 1500, 254, 12, Some(device_mtu as u32));
    poll(&net, 2002);
    frames(&device);

    enqueue(&device, echo(PEER, LOCAL, b"learned-timers"));
    poll(&net, 2100);
    let multicast = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 2);
    let multicast_mac = [0x33, 0x33, 0xff, 0, 0, 2];
    let first = frames(&device);
    assert_eq!(first.len(), 1);
    assert_probe(&first[0], multicast, multicast_mac);
    poll(&net, 2339);
    assert!(frames(&device).is_empty());
    poll(&net, 2340);
    let retry = frames(&device);
    assert_eq!(retry.len(), 1);
    assert_probe(&retry[0], multicast, multicast_mac);

    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2341);
    let replies = frames(&device);
    assert_eq!(replies.len(), 1);
    let (reply, icmp) = parse(&replies[0]);
    assert_eq!((reply.hop_limit(), icmp.message_type()), (37, 129));
    let status = || neighbor_snapshot(&net, 2).unwrap().get(0).unwrap().state;
    poll(&net, 2440);
    assert_eq!(status(), NeighborState::Reachable);
    poll(&net, 2642);
    assert_eq!(status(), NeighborState::Stale);

    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    ).unwrap();
    let peer = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 44000, 0, 2));
    endpoint.connect(peer).unwrap();
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu), Ok(1280));
    assert_eq!(endpoint.option(InternetOption::Ipv6HopLimit), Ok(37));
    assert_eq!(interface_snapshot(&net, 2).unwrap().mtu, device_mtu);
    let payload = [0; 1280];
    endpoint.set_ipv6_option(InternetOption::Ipv6DontFragment, 1).unwrap();
    assert_eq!(endpoint.send_to(&payload, None, &[]), Err(SocketError::MessageTooLarge));
    endpoint.set_ipv6_option(InternetOption::Ipv6DontFragment, 0).unwrap();
    endpoint.send_to(&payload, None, &[]).unwrap();
    frames(&device);
    poll(&net, 2643);
    let output = frames(&device);
    assert_eq!(output.len(), 2);
    for frame in &output {
        let ethernet = EthernetFrame::parse(frame).unwrap();
        let packet = Ipv6Packet::parse(ethernet.payload()).unwrap();
        assert!(packet.bytes().len() <= 1280);
        assert_eq!(packet.hop_limit(), 37);
        assert!(packet.extensions().any(|extension| extension.fragment().is_some()));
    }
    endpoint.set_ipv6_option(InternetOption::Ipv6HopLimit, 64).unwrap();
    endpoint.send_to(b"explicit", None, &[]).unwrap();
    endpoint.set_ipv6_option(InternetOption::Ipv6HopLimit, -1).unwrap();
    advertise(0, 0, 255, 49, Some(device_mtu as u32));
    poll(&net, 2644);
    let explicit = frames(&device);
    assert_eq!(explicit.len(), 1);
    assert_eq!(Ipv6Packet::parse(&explicit[0][14..]).unwrap().hop_limit(), 64);
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu), Ok(device_mtu));
    assert_eq!(endpoint.option(InternetOption::Ipv6HopLimit), Ok(49));

    let raw = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Raw(253),
        Arc::new(crate::Event::default()),
        net.clone(),
    ).unwrap();
    let raw_peer = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 0, 0, 2));
    raw.connect(raw_peer).unwrap();
    assert_eq!(raw.option(InternetOption::Ipv6HopLimit), Ok(49));
    raw.send_to(b"raw", None, &[]).unwrap();
    let tcp = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        net.clone(),
    ).unwrap();
    assert_eq!(tcp.connect(peer), Err(SocketError::InProgress));
    poll(&net, 2645);
    let output = frames(&device);
    for protocol in [6, 253] {
        let packet = output.iter().find_map(|frame| {
            let ip = Ipv6Packet::parse(&frame[14..]).ok()?;
            (ip.upper_protocol() == protocol).then_some(ip)
        }).unwrap();
        assert_eq!(packet.hop_limit(), 49);
    }
    raw.close_description();
    tcp.close_description();
    endpoint.close_description();
}
#[test]
fn unknown_peer_resolves_from_tlla_and_does_not_learn_from_data_or_unrequested_na() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2002);
    assert!(frames(&device).is_empty());
    let request = echo(PEER, LOCAL, b"saved-until-neighbor-is-known");
    enqueue(&device, request.clone());
    poll(&net, 2003);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_probe(
        &output[0],
        Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 2),
        [0x33, 0x33, 0xff, 0, 0, 2],
    );
    assert!(
        device
            .wire
            .lock()
            .unwrap()
            .2
            .received
            .iter()
            .all(Option::is_none)
    );
    // Ethernet 解析中 INCOMPLETE 不得接受没有 TLLA、广播 TLLA 或非 255 Hop Limit 的 NA。
    enqueue(&device, advertisement(PEER, None, 0x6000_0000));
    enqueue(&device, advertisement(PEER, Some([0xff; 6]), 0x6000_0000));
    let mut wrong_hops = advertisement(PEER, Some(MAC), 0x6000_0000);
    wrong_hops[21] = 254;
    enqueue(&device, wrong_hops);
    poll(&net, 2004);
    assert!(frames(&device).is_empty());
    device
        .transmit_available
        .store(false, core::sync::atomic::Ordering::Relaxed);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2005);
    assert!(frames(&device).is_empty());
    let network = stack(&net).unwrap().lock().unwrap();
    assert_eq!(
        network
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        0,
        "a retained reply is not a lost packet"
    );
    assert!(
        !network
            .interface(2)
            .unwrap()
            .ipv6
            .as_ref()
            .unwrap()
            .due(2005)
    );
    drop(network);
    device
        .transmit_available
        .store(true, core::sync::atomic::Ordering::Relaxed);
    poll(&net, 2005);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
    assert_eq!(parse(&output[0]).1.body(), &request[58..]);
    // 已解析后直接回复，不重复创建解析义务。
    enqueue(&device, request);
    poll(&net, 2006);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(parse(&output[0]).1.message_type(), 129);
}
#[test]
fn na_without_slla_first_resolves_neighbor_instead_of_guessing_l2_return_address() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let mut solicitation = vec![0; 78];
    NdMessage::NeighborSolicitation { target: LOCAL }
        .emit(&mut solicitation[54..], PEER, LOCAL)
        .unwrap();
    envelope(&mut solicitation, PEER, LOCAL, false, 255);
    enqueue(&device, solicitation);
    poll(&net, 2002);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(parse(&output[0]).1.message_type(), 135);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2003);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
    assert!(matches!(
        NdPacket::parse(parse(&output[0]).1).unwrap().message(),
        NdMessage::NeighborAdvertisement {
            target: LOCAL,
            flags: 0xe000_0000
        }
    ));
}
#[test]
fn reachable_expires_to_stale_and_usage_delays_bounded_unicast_nud_with_tx_backpressure() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    enqueue(&device, echo(PEER, LOCAL, b"start"));
    poll(&net, 2002);
    frames(&device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2003);
    frames(&device);
    // 随机 ReachableTime 为 15..45 秒；首次使用与过期时钟同轮到达，也必须从 DELAY 开始 NUD。
    enqueue(&device, echo(PEER, LOCAL, b"stale-is-usable"));
    poll(&net, 47005);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(parse(&output[0]).1.message_type(), 129);
    poll(&net, 52004);
    assert!(frames(&device).is_empty());
    device
        .transmit_available
        .store(false, core::sync::atomic::Ordering::Relaxed);
    poll(&net, 52005);
    assert!(frames(&device).is_empty());
    assert!(
        !stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .ipv6
            .as_ref()
            .unwrap()
            .due(52005)
    );
    // 容量失败不消耗三个 NUD 探测，也不被旧计时器空转重投。
    poll(&net, 62005);
    assert!(frames(&device).is_empty());
    device
        .transmit_available
        .store(true, core::sync::atomic::Ordering::Relaxed);
    for ms in [62006, 63006, 64006] {
        poll(&net, ms);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_probe(&output[0], PEER, MAC);
    }
    poll(&net, 65006);
    assert!(frames(&device).is_empty());
    enqueue(&device, echo(PEER, LOCAL, b"must-resolve-again"));
    poll(&net, 65007);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(
        parse(&output[0]).0.destination(),
        Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 2)
    );
}
#[test]
fn override_clear_preserves_known_mac_and_override_set_updates_it_without_l2_guessing() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2002);
    let changed = [2, 0, 0, 0, 0, 17];
    enqueue(&device, advertisement(PEER, Some(changed), 0x4000_0000));
    poll(&net, 2003);
    enqueue(&device, echo(PEER, LOCAL, b"override-clear"));
    poll(&net, 2004);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
    enqueue(&device, advertisement(PEER, Some(changed), 0x2000_0000));
    poll(&net, 2005);
    enqueue(&device, echo(PEER, LOCAL, b"override-set"));
    poll(&net, 2006);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(
        EthernetFrame::parse(&output[0]).unwrap().destination(),
        changed
    );
    enqueue(&device, advertisement(PEER, None, 0x4000_0000));
    poll(&net, 2007);
    enqueue(&device, echo(PEER, LOCAL, b"no-option-keeps-mac"));
    poll(&net, 2008);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(
        EthernetFrame::parse(&output[0]).unwrap().destination(),
        changed
    );
}
#[test]
fn unresolved_queue_has_a_hard_budget_flushes_in_slices_and_is_cleared_on_link_down() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    for _ in 0..65 {
        enqueue(&device, echo(PEER, LOCAL, b"bounded-wait"));
    }
    poll(&net, 2002);
    poll(&net, 2002);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(parse(&output[0]).1.message_type(), 135);
    assert_eq!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        1
    );
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    let mut received = 0;
    for _ in 0..8 {
        poll(&net, 2003);
        let output = frames(&device);
        assert!(output.len() <= 8);
        assert!(
            output
                .iter()
                .all(|frame| parse(frame).1.message_type() == 129)
        );
        received += output.len();
    }
    assert_eq!(received, 64);
    poll(&net, 2003);
    assert!(frames(&device).is_empty());
    configure_up(&net, 2, false).unwrap();
    configure_up(&net, 2, true).unwrap();
    for ms in [3000, 4000, 5000, 5001] {
        poll(&net, ms);
    }
    frames(&device);
    enqueue(&device, echo(PEER, LOCAL, b"drop-on-down"));
    poll(&net, 5002);
    frames(&device);
    configure_up(&net, 2, false).unwrap();
    configure_up(&net, 2, true).unwrap();
    for ms in [6000, 7000, 8000, 8001] {
        poll(&net, ms);
    }
    frames(&device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 8002);
    assert!(
        frames(&device).is_empty(),
        "reopening the link cannot resurrect queued data or old neighbor state"
    );
    assert!(
        device
            .wire
            .lock()
            .unwrap()
            .2
            .received
            .iter()
            .all(Option::is_none)
    );
}
#[test]
fn every_neighbor_admission_allocation_can_fail_without_publishing_half_indexes_or_waiters() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    // 依次拒绝载荷、等待节点、时钟存储和邻居节点；报文在故障注入前放入 NIC，避免测试边界分配干扰。
    for fault in 0..4 {
        enqueue(&device, echo(PEER, LOCAL, b"allocation-failure"));
        crate::allocation_failure::fail_after(fault);
        poll(&net, 2002);
        assert!(crate::allocation_failure::was_consumed());
        assert!(frames(&device).is_empty());
    }
    assert_eq!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        4
    );
    enqueue(&device, echo(PEER, LOCAL, b"only-this-one-survives"));
    poll(&net, 2003);
    frames(&device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2004);
    let output = frames(&device);
    assert_eq!(output.len(), 1);
    assert_eq!(&parse(&output[0]).1.body()[4..], b"only-this-one-survives");
    poll(&net, 7004);
    assert!(frames(&device).is_empty());
    let other = "fe80::3".parse().unwrap();
    enqueue(&device, echo(other, LOCAL, b"unresolved-timeout"));
    for ms in [8000, 9000, 10000] {
        poll(&net, ms);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_eq!(parse(&output[0]).1.message_type(), 135);
    }
    poll(&net, 11000);
    assert_eq!(
        stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        5
    );
    enqueue(&device, advertisement(other, Some(MAC), 0x6000_0000));
    poll(&net, 11001);
    assert!(
        frames(&device).is_empty(),
        "failed address resolution must retire the queued payload and timer"
    );
}

#[test]
fn cached_echo_and_stale_to_delay_timer_rearm_do_not_allocate_payload_or_index_nodes() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    learn_peer(&net, &device);
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2002);
    poll(&net, 47003);
    assert!(frames(&device).is_empty());
    let request = echo(PEER, LOCAL, b"allocation-free-cached-reply");
    let mut output = [0; 1514];
    let mut length = 0;
    let mut dropped = 0;
    let mut network = stack(&net).unwrap().lock().unwrap();
    let route = network
        .ipv6_routing
        .select(LOCAL, PEER, Some(2), 0, &network.interfaces)
        .unwrap();
    let ipv6 = network.interface_mut(2).unwrap().ipv6.as_mut().unwrap();
    // 物理出帧已由上面的 wire 用例覆盖；这里用固定 TX 存储排除 NIC 记录器自己的 Vec 分配。
    crate::allocation_failure::fail_next();
    let packet = crate::inet::ipv6::incoming(&request, [2, 0, 0, 0, 0, 1], 1500, |destination| {
        ipv6.accepts_destination(destination)
    })
    .unwrap();
    let packet = packet_wire::ipv6::Ipv6Packet::parse(packet.bytes()).unwrap();
    let received = ipv6.receive(
        packet,
        ipv6.has_address(packet.destination()),
        true,
        ([2, 0, 0, 0, 0, 1], 1500),
        47004,
        (|_| panic!("Echo must return to the IP route owner"),
        || dropped += 1),
    );
    ipv6.send_reply(
        received.reply.unwrap(),
        route.neighbor,
        [2, 0, 0, 0, 0, 1],
        47004,
        |packet| {
            length = packet.length();
            packet
                .emit(&mut output[..length], [2, 0, 0, 0, 0, 1])
                .unwrap();
            true
        },
        || dropped += 1,
    );
    assert!(!crate::allocation_failure::was_consumed());
    assert_eq!(dropped, 0);
    assert_eq!(parse(&output[..length]).1.message_type(), 129);
}

// 原有出帧断言不能证明只读查询不会变更邻居，或冻结结果在 link-down/OOM 后仍保持一致。
#[test]
fn neighbor_query_freezes_owner_facts_filters_interface_and_preserves_router_flag_rules() {
    use network_protocol::NeighborState;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    assert!(neighbor_snapshot(&net, 0).unwrap().is_empty());
    assert!(matches!(
        neighbor_snapshot(&net, 99),
        Err(SocketError::NoDevice)
    ));
    enqueue(&device, echo(PEER, LOCAL, b"resolve"));
    poll(&net, 2002);
    frames(&device);
    let frozen = neighbor_snapshot(&net, 2).unwrap();
    let pending = frozen.get(0).unwrap();
    assert_eq!(
        (
            pending.interface_index,
            pending.address,
            pending.hardware_address,
            pending.state,
            pending.probes
        ),
        (2, PEER.into(), None, NeighborState::Incomplete, 1)
    );
    assert!(neighbor_snapshot(&net, 1).unwrap().is_empty());
    let isolated = crate::namespace::Namespace::create().unwrap();
    isolated.install_protocol(prepare_network(&isolated, &[], true).unwrap());
    assert!(neighbor_snapshot(&isolated, 0).unwrap().is_empty());
    enqueue(&device, advertisement(PEER, Some(MAC), 0xe000_0000));
    poll(&net, 2003);
    frames(&device);
    let row = neighbor_snapshot(&net, 0).unwrap().get(0).unwrap();
    assert_eq!(
        (row.hardware_address, row.state, row.is_router),
        (Some(MAC), NeighborState::Reachable, true)
    );
    // Override 清除且 TLLA 改变的 NA 不能借 Router 位修改被忽略的邻居身份。
    enqueue(
        &device,
        advertisement(PEER, Some([2, 0, 0, 0, 0, 3]), 0x4000_0000),
    );
    poll(&net, 2004);
    let retained = neighbor_snapshot(&net, 2).unwrap().get(0).unwrap();
    assert_eq!(
        (
            retained.hardware_address,
            retained.state,
            retained.is_router
        ),
        (Some(MAC), NeighborState::Stale, true)
    );
    enqueue(&device, advertisement(PEER, Some(MAC), 0x6000_0000));
    poll(&net, 2005);
    let current = neighbor_snapshot(&net, 2).unwrap().get(0).unwrap();
    assert!(!current.is_router);
    crate::allocation_failure::fail_next();
    assert!(matches!(
        neighbor_snapshot(&net, 0),
        Err(SocketError::NoMemory)
    ));
    assert!(crate::allocation_failure::was_consumed());
    assert_eq!(neighbor_snapshot(&net, 2).unwrap().get(0), Some(current));
    configure_up(&net, 2, false).unwrap();
    assert!(neighbor_snapshot(&net, 0).unwrap().is_empty());
    assert_eq!(frozen.get(0), Some(pending));
}
