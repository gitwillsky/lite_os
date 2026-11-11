// 实际 InetSocket、原生连接、port leases 与 deferred cleanup；只有 NIC/时钟/唤醒为边界。

#[test]
fn passive_child_freezes_final_ack_policy_and_failed_accept_keeps_the_same_child() {
    use smoltcp::wire::TcpControl;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    listener.bind_to_device(b"eth0", false).unwrap();
    listener.bind(address(43007)).unwrap();
    listener.set_hop_limit(Some(33)).unwrap();
    listener.set_max_segment(512).unwrap();
    listener.listen(2).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43007, TcpControl::Syn, None));
    {
        use smoltcp::wire::*;
        let wire = WIRE.lock().unwrap();
        assert!(
            wire.1.iter().any(|frame| {
                let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
                if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                    return false;
                }
                let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
                if ip.next_header() != IpProtocol::Tcp {
                    return false;
                }
                let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                tcp.src_port() == 43007
                    && tcp.syn()
                    && tcp.ack()
                    && ip.hop_limit() == 33
                    && TcpRepr::parse(
                        &tcp,
                        &IpAddress::Ipv4(ip.src_addr()),
                        &IpAddress::Ipv4(ip.dst_addr()),
                        &smoltcp::phy::ChecksumCapabilities::default(),
                    )
                    .unwrap()
                    .max_seg_size
                        == Some(512)
            }),
            "each real listening handle must use configured TTL before first SYN/ACK"
        );
    }
    let ack = syn_ack(43007);
    assert!(
        !listener.poll_state().readable,
        "SYN alone does not create accepted policy"
    );
    listener.set_no_delay(true).unwrap();
    listener.set_hop_limit(Some(41)).unwrap();
    listener.set_max_segment(256).unwrap();
    listener.set_reuse_address(true).unwrap();
    ingress(peer_tcp_frame(43007, TcpControl::None, Some(ack)));
    assert!(listener.poll_state().readable);
    listener.set_no_delay(false).unwrap();
    listener.set_hop_limit(Some(55)).unwrap();
    listener.set_max_segment(1024).unwrap();
    listener.set_reuse_address(false).unwrap();
    // parent 改绑需为冻结 child 留住原设备端口；逐个故障点验证同一事务的回滚。
    for successful in 0..4 {
        crate::allocation_failure::fail_after(successful);
        assert_eq!(
            listener.bind_to_device(b"lo", true),
            Err(SocketError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(listener.option(InternetOption::BoundToDevice), Ok(2));
        assert!(listener.poll_state().readable);
    }
    listener.bind_to_device(b"lo", true).unwrap();
    let blocked = scoped_endpoint(namespace(), InetProtocol::Tcp);
    blocked.bind_to_device(b"eth0", false).unwrap();
    assert_eq!(blocked.bind(address(43007)), Err(SocketError::AddressInUse));
    blocked.close_description();
    let notify = Arc::new(crate::Event::default());
    crate::allocation_failure::fail_next();
    assert!(matches!(
        listener.accept(notify),
        Err(SocketError::NoMemory)
    ));
    assert!(crate::allocation_failure::was_consumed());
    assert!(
        listener.poll_state().readable,
        "OOM must retain actual child and frozen policy"
    );
    let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
    let InetEndpoint::Tcp(id) = child.endpoint else {
        unreachable!()
    };
    {
        let network = initial_stack().unwrap().lock().unwrap();
        assert!(network.tcp_endpoints[&id].options.no_delay);
        assert!(network.tcp_endpoints[&id].options.reuse_address);
        assert!(tcp::transport_options_match(&network, id, true, Some(41)));
        let InetEndpoint::Tcp(listener_id) = listener.endpoint else {
            unreachable!()
        };
        assert!(tcp::transport_options_match(
            &network,
            listener_id,
            false,
            Some(55)
        ));
    }
    assert_eq!(child.option(InternetOption::BoundToDevice), Ok(2));
    assert_eq!(listener.option(InternetOption::BoundToDevice), Ok(1));
    assert_eq!(child.option(InternetOption::HopLimit).unwrap(), 41);
    assert_eq!(listener.option(InternetOption::HopLimit).unwrap(), 55);
    assert_eq!(child.option(InternetOption::NoDelay).unwrap(), 1);
    assert_eq!(listener.option(InternetOption::NoDelay).unwrap(), 0);
    assert_eq!(child.option(InternetOption::MaxSegment).unwrap(), 512);
    assert_eq!(listener.option(InternetOption::MaxSegment).unwrap(), 1024);
    // SYN 时的真实广告已固定；ESTABLISHED setter 只改变下一次 open policy，不伪造当前 MSS。
    child.set_max_segment(88).unwrap();
    assert_eq!(child.option(InternetOption::MaxSegment).unwrap(), 512);
    WIRE.lock().unwrap().1.clear();
    assert_eq!(child.send_to(&vec![0x35; 2000], None, &[]).unwrap(), 2000);
    initial_stack().unwrap().lock().unwrap().poll();
    {
        use smoltcp::wire::*;
        let wire = WIRE.lock().unwrap();
        let mut bytes = 0;
        for frame in &wire.1 {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                continue;
            }
            let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp {
                continue;
            }
            let packet = TcpPacket::new_checked(ip.payload()).unwrap();
            if packet.src_port() == 43007 && !packet.payload().is_empty() {
                assert!(packet.payload().len() <= 512);
                bytes += packet.payload().len();
            }
        }
        assert!(
            bytes >= 512,
            "actual sender must transmit data through negotiated MSS"
        );
    }
    ingress(peer_tcp_frame(43007, TcpControl::Rst, Some(ack)));
    child.close_description();
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}
// ACK 与 RST 故意在同一次实际 NIC poll 中交付，不把问题拆成 fixture 的状态变化。
#[test]
fn final_ack_and_reset_in_one_batch_preserve_accept_binding_and_one_error() {
    use smoltcp::wire::TcpControl;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    listener.bind(address(43008)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43008, TcpControl::Syn, None));
    let ack = syn_ack(43008);
    {
        let mut wire = WIRE.lock().unwrap();
        wire.0
            .push_back(peer_tcp_frame(43008, TcpControl::None, Some(ack)));
        wire.0
            .push_back(peer_tcp_frame(43008, TcpControl::Rst, Some(ack)));
    }
    initial_stack().unwrap().lock().unwrap().poll();
    assert!(
        listener.poll_state().readable,
        "established reset child remains in accept queue"
    );
    let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
    assert_eq!(
        child.address().unwrap(),
        InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 2, 15),
            43008
        ))
    );
    assert_eq!(child.peer_address(), Err(SocketError::NotConnected));
    let mut empty = [];
    assert_eq!(receive(&child, &mut empty, false).unwrap().0, 0);
    let mut byte = [0];
    assert!(matches!(
        receive(&child, &mut byte, false),
        Err(SocketError::ConnectionReset)
    ));
    assert_eq!(child.take_error(), None);
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(
        child.take_error(),
        None,
        "subsequent maintain cannot recreate RESET"
    );
    assert_eq!(receive(&child, &mut byte, false).unwrap().0, 0);
    assert_eq!(
        child.send_to(b"x", None, &[]),
        Err(SocketError::BrokenPipe)
    );
    assert!(child.poll_state().hangup && child.poll_state().writable);
    assert!(!child.poll_state().error);
    child.close_description();
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}
