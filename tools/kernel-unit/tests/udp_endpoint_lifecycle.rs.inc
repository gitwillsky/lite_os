// 载入实际 Inet/UDP transport/port namespace；只有标准 wire、NIC 和 scheduler 是输入边界。
fn udp_endpoint() -> Arc<InetSocket> {
    InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::socket::SocketMemory::try_new(crate::socket::initial_net()).unwrap(),
    )
    .unwrap()
}
fn udp_peer(port: u16) -> InetAddress {
    InetAddress {
        address: Ipv4Addr::new(10, 0, 2, 16),
        port,
    }
}
fn udp_read(
    endpoint: &InetSocket,
    peek: bool,
) -> Result<(Vec<u8>, Option<InetAddress>), SocketError> {
    let mut data = [0u8; 1500]; // 包含 lo MTU 回归的完整 datagram，截断行为由专门用例验证。
    let (copied, full, source, _) =
        endpoint.receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut data), peek)?;
    assert_eq!(copied, full);
    Ok((data[..copied].to_vec(), source))
}
#[test]
fn udp_peer_is_checked_before_enqueue_and_reconnect_preserves_accepted_packets() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = udp_endpoint();
    endpoint.bind(address(43100)).unwrap();
    ingress(peer_udp_frame(udp_peer(44001), 43100, b"old"));
    endpoint.connect(udp_peer(44000)).unwrap();
    // 非 peer 满批不能占据 queue 容量或触发 receiver 的新 readable edge。
    for _ in 0..12 {
        ingress(peer_udp_frame(udp_peer(44001), 43100, b"rejected"));
    }
    ingress(peer_udp_frame(udp_peer(44000), 43100, b"first"));
    endpoint.connect(udp_peer(44002)).unwrap();
    ingress(peer_udp_frame(udp_peer(44000), 43100, b"rejected"));
    ingress(peer_udp_frame(udp_peer(44002), 43100, b"second"));
    assert_eq!(
        udp_read(&endpoint, false).unwrap(),
        (b"old".to_vec(), Some(udp_peer(44001)))
    );
    assert_eq!(
        udp_read(&endpoint, true).unwrap(),
        (b"first".to_vec(), Some(udp_peer(44000)))
    );
    assert_eq!(
        udp_read(&endpoint, false).unwrap(),
        (b"first".to_vec(), Some(udp_peer(44000)))
    );
    assert_eq!(
        udp_read(&endpoint, false).unwrap(),
        (b"second".to_vec(), Some(udp_peer(44002)))
    );
    assert_eq!(udp_read(&endpoint, false), Err(SocketError::Again));
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn udp_disconnect_preserves_queue_and_only_releases_unlocked_local_tuple() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for explicit in [false, true] {
        let endpoint = udp_endpoint();
        if explicit {
            endpoint.bind(address(43101)).unwrap();
        }
        endpoint.connect(udp_peer(44000)).unwrap();
        let local = endpoint.address().unwrap();
        ingress(peer_udp_frame(udp_peer(44000), local.port, b"retained"));
        endpoint.disconnect().unwrap();
        assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
        assert_eq!(
            endpoint.address().unwrap(),
            address(if explicit { 43101 } else { 0 })
        );
        assert_eq!(udp_read(&endpoint, false).unwrap().0, b"retained");
        if explicit {
            ingress(peer_udp_frame(udp_peer(44001), local.port, b"unconnected"));
            assert_eq!(udp_read(&endpoint, false).unwrap().0, b"unconnected");
        } else {
            let replacement = udp_endpoint();
            replacement.bind(address(local.port)).unwrap();
            replacement.close_description();
        }
        endpoint.close_description();
    }
    let endpoint = udp_endpoint();
    endpoint
        .bind(InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 15),
            port: 0,
        })
        .unwrap();
    endpoint.connect(udp_peer(44000)).unwrap();
    endpoint.disconnect().unwrap();
    assert_eq!(
        endpoint.address().unwrap(),
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 15),
            port: 0
        }
    );
    endpoint.connect(udp_peer(44000)).unwrap();
    assert_ne!(endpoint.address().unwrap().port, 0);
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn udp_shutdown_is_native_policy_and_read_eof_does_not_disable_new_ingress() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = udp_endpoint();
    endpoint.bind(address(43102)).unwrap();
    assert_eq!(endpoint.shutdown(0), Err(SocketError::NotConnected));
    let state = endpoint.poll_state();
    assert!(state.readable && state.read_closed && !state.hangup);
    assert_eq!(udp_read(&endpoint, false).unwrap(), (Vec::new(), None));
    endpoint.connect(udp_peer(44000)).unwrap();
    ingress(peer_udp_frame(udp_peer(44000), 43102, b"after-eof"));
    assert_eq!(udp_read(&endpoint, true).unwrap().0, b"after-eof");
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"after-eof");
    assert_eq!(udp_read(&endpoint, false).unwrap(), (Vec::new(), None));
    endpoint.shutdown(1).unwrap();
    assert!(endpoint.poll_state().hangup);
    assert_eq!(endpoint.send_to(b"x", None, None), Err(SocketError::BrokenPipe));
    endpoint.disconnect().unwrap();
    endpoint.connect(udp_peer(44000)).unwrap();
    assert!(endpoint.poll_state().read_closed && endpoint.poll_state().hangup);
    assert_eq!(endpoint.send_to(b"x", None, None), Err(SocketError::BrokenPipe));
    assert_eq!(endpoint.shutdown(3), Err(SocketError::Invalid));
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn udp_reuse_demux_uses_actual_bind_order_and_connected_tuple_not_socket_creation() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let first = udp_endpoint();
    let second = udp_endpoint();
    first.set_reuse_address(true).unwrap();
    second.set_reuse_address(true).unwrap();
    second.bind(address(43103)).unwrap();
    first.bind(address(43103)).unwrap();
    ingress(peer_udp_frame(udp_peer(44000), 43103, b"last-bind"));
    assert_eq!(udp_read(&first, false).unwrap().0, b"last-bind");
    assert_eq!(udp_read(&second, false), Err(SocketError::Again));
    second.connect(udp_peer(44000)).unwrap();
    ingress(peer_udp_frame(udp_peer(44000), 43103, b"connected"));
    assert_eq!(udp_read(&second, false).unwrap().0, b"connected");
    ingress(peer_udp_frame(udp_peer(44001), 43103, b"fallback"));
    assert_eq!(udp_read(&first, false).unwrap().0, b"fallback");
    first.close_description();
    second.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn udp_already_enqueued_transmit_retains_source_and_ttl_after_policy_change_and_disconnect() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    ingress(peer_arp_frame());
    let endpoint = udp_endpoint();
    endpoint.connect(udp_peer(44000)).unwrap();
    endpoint.set_hop_limit(Some(37)).unwrap();
    let original = endpoint.address().unwrap();
    assert_eq!(endpoint.send_to(b"frozen", None, None), Ok(6));
    endpoint.set_hop_limit(Some(55)).unwrap();
    assert_eq!(
        endpoint
            .option(super::super::InternetOption::HopLimit)
            .unwrap(),
        55
    );
    endpoint.disconnect().unwrap();
    assert_eq!(endpoint.address().unwrap().port, 0);
    initial_stack().unwrap().lock().unwrap().poll();
    let wire = crate::drivers::WIRE.lock().unwrap();
    let delivered = wire.1.iter().any(|bytes| {
        let ethernet = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Udp {
            return false;
        }
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        udp.src_port() == original.port
            && udp.dst_port() == 44000
            && udp.payload() == b"frozen"
            && ip.hop_limit() == 37
    });
    assert!(delivered, "TX source belongs to already enqueued packet");
    drop(wire);
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn udp_broadcast_delivers_all_matching_sockets_without_creating_a_recipient_queue() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let first = udp_endpoint();
    let second = udp_endpoint();
    first.set_reuse_address(true).unwrap();
    second.set_reuse_address(true).unwrap();
    first.bind(address(43104)).unwrap();
    second.bind(address(43104)).unwrap();
    ingress(peer_udp_frame_for(
        udp_peer(44000),
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 255),
            port: 43104,
        },
        b"broadcast",
    ));
    assert_eq!(udp_read(&first, false).unwrap().0, b"broadcast");
    assert_eq!(udp_read(&second, false).unwrap().0, b"broadcast");
    first.close_description();
    second.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
