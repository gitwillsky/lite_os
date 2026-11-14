// 载入实际 Inet/UDP transport/port namespace；只有标准 wire、NIC 和 scheduler 是输入边界。
fn udp_endpoint() -> Arc<InetSocket> {
    InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap()
}
fn udp_peer(port: u16) -> InetAddress {
    InetAddress::V4(core::net::SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 2, 16),
        port,
    ))
}
fn udp_read(
    endpoint: &InetSocket,
    peek: bool,
) -> Result<(Vec<u8>, Option<InetAddress>), SocketError> {
    let mut data = crate::receive::ReceiveBuffer::try_new(1500).unwrap(); // 包含 lo MTU 回归的完整 datagram，截断行为由专门用例验证。
    let (copied, full, source, _) = endpoint.receive(&mut data, peek)?;
    assert_eq!(copied, full);
    Ok((data.into_bytes(), source))
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
        ingress(peer_udp_frame(udp_peer(44000), local.port(), b"retained"));
        endpoint.disconnect().unwrap();
        assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
        assert_eq!(
            endpoint.address().unwrap(),
            address(if explicit { 43101 } else { 0 })
        );
        assert_eq!(udp_read(&endpoint, false).unwrap().0, b"retained");
        if explicit {
            ingress(peer_udp_frame(
                udp_peer(44001),
                local.port(),
                b"unconnected",
            ));
            assert_eq!(udp_read(&endpoint, false).unwrap().0, b"unconnected");
        } else {
            let replacement = udp_endpoint();
            replacement.bind(address(local.port())).unwrap();
            replacement.close_description();
        }
        endpoint.close_description();
    }
    let endpoint = udp_endpoint();
    endpoint
        .bind(InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 2, 15),
            0,
        )))
        .unwrap();
    endpoint.connect(udp_peer(44000)).unwrap();
    endpoint.disconnect().unwrap();
    assert_eq!(
        endpoint.address().unwrap(),
        InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), 0))
    );
    endpoint.connect(udp_peer(44000)).unwrap();
    assert_ne!(endpoint.address().unwrap().port(), 0);
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
    assert_eq!(
        endpoint.send_to(b"x", None, &[]),
        Err(SocketError::BrokenPipe)
    );
    endpoint.disconnect().unwrap();
    endpoint.connect(udp_peer(44000)).unwrap();
    assert!(endpoint.poll_state().read_closed && endpoint.poll_state().hangup);
    assert_eq!(
        endpoint.send_to(b"x", None, &[]),
        Err(SocketError::BrokenPipe)
    );
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
    assert_eq!(endpoint.send_to(b"frozen", None, &[]), Ok(6));
    endpoint.set_hop_limit(Some(55)).unwrap();
    assert_eq!(endpoint.option(InternetOption::HopLimit).unwrap(), 55);
    endpoint.disconnect().unwrap();
    assert_eq!(endpoint.address().unwrap().port(), 0);
    initial_stack().unwrap().lock().unwrap().poll();
    let wire = WIRE.lock().unwrap();
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
        udp.src_port() == original.port()
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
    let broadcast_peer = InetAddress::V4(core::net::SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 2, 255),
        44000,
    ));
    assert_eq!(
        first.connect(broadcast_peer),
        Err(SocketError::PermissionDenied)
    );
    first.set_broadcast(true).unwrap();
    first.connect(broadcast_peer).unwrap();
    first.disconnect().unwrap();
    ingress(peer_udp_frame_for(
        udp_peer(44000),
        InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 2, 255),
            43104,
        )),
        b"broadcast",
    ));
    assert_eq!(udp_read(&first, false).unwrap().0, b"broadcast");
    assert_eq!(udp_read(&second, false).unwrap().0, b"broadcast");
    first.close_description();
    second.close_description();
    assert_eq!(counts(), (0, 0, 0));
    // 无 IPv4 地址时 DHCP 的 limited broadcast 仍可用；mapped IPv6 不能把 wildcard 选源当成 exact lease。
    let scope = crate::namespace::Namespace::create().unwrap();
    let nic: Arc<dyn device_model::network::NetworkDevice> = Arc::new(WireDevice::new());
    scope.install_protocol(prepare_network(&scope, &[nic], true).unwrap());
    configure_up(&scope, 2, true).unwrap();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        scope.clone(),
    )
    .unwrap();
    endpoint.bind_to_device(b"eth0", false).unwrap();
    endpoint.set_broadcast(true).unwrap();
    endpoint
        .connect("[::ffff:255.255.255.255]:67".parse().unwrap())
        .unwrap();
    assert_eq!(
        endpoint.address().unwrap().ip(),
        core::net::IpAddr::V6(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped())
    );
    endpoint.send_to(b"discover", None, &[]).unwrap();
    advance_actual_domains();
    endpoint.disconnect().unwrap();
    endpoint.close_description();
    assert!(
        scope
            .protocol()
            .unwrap()
            .lock()
            .unwrap()
            .udp_endpoints
            .is_empty()
    );
}

#[test]
fn second_receive_queue_delivers_udp_and_only_reserves_tx_for_a_reply() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let scope = crate::namespace::Namespace::create().unwrap();
    let mut nic = WireDevice::new();
    nic.queue_count = 2;
    nic.checksum = true;
    nic.receive_on
        .store(1, core::sync::atomic::Ordering::Relaxed);
    let nic = Arc::new(nic);
    let device: Arc<dyn device_model::network::NetworkDevice> = nic.clone();
    scope.install_protocol(prepare_network(&scope, core::slice::from_ref(&device), false).unwrap());
    configure_address(&scope, 2, Ipv4Addr::new(10, 0, 2, 15)).unwrap();
    configure_netmask(&scope, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&scope, 2, true).unwrap();
    let endpoint = scoped_endpoint(&scope, InetProtocol::Udp);
    endpoint.bind(address(43177)).unwrap();
    let taps: Vec<_> = [
        network_protocol::PacketKind::Raw,
        network_protocol::PacketKind::Datagram,
    ]
    .into_iter()
    .map(|kind| {
        crate::packet::PacketSocket::new(
            kind,
            0x0800u16.to_be(),
            Arc::new(crate::Event::default()),
            scope.clone(),
        )
        .unwrap()
    })
    .collect();
    nic.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_udp_frame(udp_peer(44017), 43177, b"queue-one"));
    let mut scan = crate::namespace::DomainScan::new(0, 1);
    for _ in 0..4 {
        dispatch_network_work(&mut scan, u64::MAX);
    }
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"queue-one");
    assert_eq!(
        nic.cancelled_queue
            .load(core::sync::atomic::Ordering::Relaxed),
        u16::MAX
    );
    nic.wire.lock().unwrap().0.push_back(peer_arp_frame_for(
        Ipv4Addr::new(10, 0, 2, 15),
        ipv4_address(udp_peer(44017)),
    ));
    advance_actual_domains();
    assert_eq!(nic.wire.lock().unwrap().2.last_transmit_queue, Some(1));
    // 先消费接收镜像；出口镜像必须独立携带尚待硬件补全的状态。
    for tap in &taps {
        let mut bytes = [0; 1600];
        let (_, _, source, information) = receive(tap, &mut bytes, false).unwrap();
        assert_ne!(source.packet_type, 4);
        assert_eq!(
            information.checksum,
            network_protocol::PacketChecksum::Unverified
        );
    }
    endpoint
        .send_to(b"offload-udp", Some(udp_peer(44017)), &[])
        .unwrap();
    advance_actual_domains();
    let wire = nic.wire.lock().unwrap();
    let frame = wire
        .1
        .iter()
        .find_map(|bytes| {
            let eth = smoltcp::wire::EthernetFrame::new_checked(bytes.as_slice()).ok()?;
            let ip = smoltcp::wire::Ipv4Packet::new_checked(eth.payload()).ok()?;
            (ip.next_header() == smoltcp::wire::IpProtocol::Udp).then_some(bytes)
        })
        .unwrap();
    let eth = smoltcp::wire::EthernetFrame::new_checked(frame.as_slice()).unwrap();
    let ip = smoltcp::wire::Ipv4Packet::new_checked(eth.payload()).unwrap();
    let udp = smoltcp::wire::UdpPacket::new_checked(ip.payload()).unwrap();
    assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
    assert_eq!(udp.payload(), b"offload-udp");
    drop(wire);
    for (tap, network_offset) in taps.iter().zip([14, 0]) {
        let mut short = [0; 4];
        let (count, full, source, information) = receive(tap, &mut short, true).unwrap();
        assert_eq!(count, short.len());
        assert_eq!(
            full,
            usize::from(network_offset) + 20 + 8 + b"offload-udp".len()
        );
        assert_eq!(source.packet_type, 4);
        assert_eq!(
            information.checksum,
            network_protocol::PacketChecksum::Partial
        );
        assert_eq!(information.network_offset, network_offset);
        assert_ne!(information.timestamp_ns, 0);
        let mut complete = [0; 1600];
        let (_, consumed_full, _, consumed_information) =
            receive(tap, &mut complete, false).unwrap();
        assert_eq!((consumed_full, consumed_information), (full, information));
    }
    // 硬件验证只属于这一帧；后续未验证包必须重新做软件校验，IP header 始终验证。
    let mut verified = peer_udp_frame(udp_peer(44017), 43177, b"hardware");
    verified[40..42].copy_from_slice(&0x1234u16.to_be_bytes());
    nic.receive_checksum_valid
        .store(true, core::sync::atomic::Ordering::Relaxed);
    nic.wire.lock().unwrap().0.push_back(verified.clone());
    advance_actual_domains();
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"hardware");
    for tap in &taps {
        let mut bytes = [0; 1600];
        let (_, _, _, information) = receive(tap, &mut bytes, false).unwrap();
        assert_eq!(
            information.checksum,
            network_protocol::PacketChecksum::Verified
        );
    }
    nic.receive_checksum_valid
        .store(false, core::sync::atomic::Ordering::Relaxed);
    nic.wire.lock().unwrap().0.push_back(verified.clone());
    advance_actual_domains();
    assert_eq!(udp_read(&endpoint, false), Err(SocketError::Again));
    nic.receive_checksum_valid
        .store(true, core::sync::atomic::Ordering::Relaxed);
    verified[24] ^= 1;
    nic.wire.lock().unwrap().0.push_back(verified);
    advance_actual_domains();
    assert_eq!(udp_read(&endpoint, false), Err(SocketError::Again));
    for tap in taps {
        tap.close_description();
    }
    endpoint.close_description();
}

// 双族服务地址不能让 IPv4 后端吞掉 zone 或把 IPv6 地址截断后改写实际绑定。
#[test]
fn ipv4_socket_family_rejection_preserves_live_tuple_and_port_membership() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let ipv6 = InetAddress::V6(core::net::SocketAddrV6::new(
        "fe80::1".parse().unwrap(),
        45000,
        0x12345678,
        2,
    ));
    for protocol in [InetProtocol::Udp, InetProtocol::Tcp] {
        let endpoint = scoped_endpoint(namespace(), protocol);
        assert_eq!(
            endpoint.bind(ipv6),
            Err(SocketError::AddressFamilyNotSupported)
        );
        assert_eq!(endpoint.address().unwrap().port(), 0);
        endpoint.bind(address(43271)).unwrap();
        let bound = endpoint.address().unwrap();
        assert_eq!(
            endpoint.connect(ipv6),
            Err(SocketError::AddressFamilyNotSupported)
        );
        assert_eq!(endpoint.address().unwrap(), bound);
        endpoint.close_description();
    }
}

#[test]
fn udp_connect_refines_port_lease_and_failed_refinement_preserves_binding() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = udp_endpoint();
    endpoint.bind(address(43105)).unwrap();
    crate::allocation_failure::fail_next();
    let result = endpoint.connect(udp_peer(44000));
    assert!(crate::allocation_failure::was_consumed());
    assert_eq!(result, Err(SocketError::NoMemory));
    assert_eq!(endpoint.address().unwrap(), address(43105));
    assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
    let other = udp_endpoint();
    let loopback = InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 43105));
    assert_eq!(other.bind(loopback), Err(SocketError::AddressInUse));
    endpoint.connect(udp_peer(44000)).unwrap();
    other.bind(loopback).unwrap();
    other.close_description();
    ingress(peer_udp_frame(udp_peer(44000), 43105, b"retained"));
    endpoint.disconnect().unwrap();
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"retained");
    let blocked = udp_endpoint();
    assert_eq!(blocked.bind(loopback), Err(SocketError::AddressInUse));
    blocked.close_description();
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn native_udp_ring_preserves_wrapped_packets_peek_zero_length_and_backpressure() {
    let _serial = SERIAL.lock().unwrap();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    let source = InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 43106));
    let target = InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 43107));
    sender.bind(source).unwrap();
    receiver.bind(target).unwrap();
    receiver.set_packet_info(true).unwrap();
    let first = vec![0x11; 60_000];
    let second = vec![0x22; 60_000];
    let third = vec![0x33; 60_000];
    for bytes in [&first, &second] {
        sender.send_to(bytes, Some(target), &[]).unwrap();
    }
    for _ in 0..3 {
        net.protocol().unwrap().lock().unwrap().poll();
    }
    let mut output = crate::receive::ReceiveBuffer::try_new(first.len()).unwrap();
    assert_eq!(
        receiver.receive(&mut output, false).unwrap(),
        (
            first.len(),
            first.len(),
            Some(source),
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 1,
                specific_address: Ipv4Addr::LOCALHOST,
                destination: Ipv4Addr::LOCALHOST
            })
        )
    );
    assert_eq!(output.into_bytes(), first);
    sender.send_to(&third, Some(target), &[]).unwrap();
    for _ in 0..3 {
        net.protocol().unwrap().lock().unwrap().poll();
    }
    // RX 的 B、C 跨越 ring 尾部；满载荷仍能排队零字节报文，但元数据最多 8 条。
    for _ in 0..7 {
        sender.send_to(&[], Some(target), &[]).unwrap();
        for _ in 0..2 {
            net.protocol().unwrap().lock().unwrap().poll();
        }
    }
    let mut peek = crate::receive::ReceiveBuffer::try_new(17).unwrap();
    let (copied, full, peer, _) = receiver.receive(&mut peek, true).unwrap();
    assert_eq!((copied, full, peer), (17, 60_000, Some(source)));
    assert_eq!(peek.into_bytes(), vec![0x22; 17]);
    for bytes in [second, third] {
        let mut output = crate::receive::ReceiveBuffer::try_new(bytes.len()).unwrap();
        let (copied, full, peer, _) = receiver.receive(&mut output, false).unwrap();
        assert_eq!(
            (copied, full, peer),
            (bytes.len(), bytes.len(), Some(source))
        );
        assert_eq!(output.into_bytes(), bytes);
    }
    for _ in 0..6 {
        let mut output = crate::receive::ReceiveBuffer::try_new(0).unwrap();
        let (copied, full, peer, _) = receiver.receive(&mut output, false).unwrap();
        assert_eq!(
            (copied, full, peer),
            (0, 0, Some(source)),
            "zero datagram is not shutdown EOF"
        );
    }
    assert!(!receiver.poll_state().readable);
    for _ in 0..8 {
        sender.send_to(&[], Some(target), &[]).unwrap();
    }
    assert!(!sender.poll_state().writable);
    assert_eq!(
        sender.send_to(&[], Some(target), &[]),
        Err(SocketError::Again)
    );
    for _ in 0..3 {
        net.protocol().unwrap().lock().unwrap().poll();
    }
    assert!(sender.poll_state().writable);
    sender.close_description();
    receiver.close_description();
}

#[test]
fn native_udp_lazy_storage_failure_releases_binding_and_does_not_publish_send() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for boundary in 0..5 {
        let endpoint = udp_endpoint();
        crate::allocation_failure::fail_after(boundary);
        let result = endpoint.bind(address(43108));
        let failed = crate::allocation_failure::was_consumed();
        if failed {
            assert_eq!(result, Err(SocketError::NoMemory));
            assert_eq!(endpoint.address().unwrap().port(), 0);
            let replacement = udp_endpoint();
            replacement.bind(address(43108)).unwrap();
            replacement.close_description();
        } else {
            assert_eq!(result, Ok(()));
        }
        endpoint.close_description();
        if !failed {
            break;
        }
    }
    for boundary in 0..5 {
        let endpoint = udp_endpoint();
        endpoint.bind(address(43108)).unwrap();
        crate::allocation_failure::fail_after(boundary);
        let result = endpoint.send_to(b"atomic", Some(udp_peer(44000)), &[]);
        let failed = crate::allocation_failure::was_consumed();
        if failed {
            assert_eq!(result, Err(SocketError::NoMemory));
            assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
            assert!(
                initial_stack()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .udp_deadline()
                    .is_none()
            );
            assert_eq!(
                endpoint.send_to(b"atomic", Some(udp_peer(44000)), &[]),
                Ok(6)
            );
        } else {
            assert_eq!(result, Ok(6));
        }
        endpoint.close_description();
        if !failed {
            break;
        }
    }
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn native_udp_submit_backpressure_retains_queue_until_device_activity() {
    use core::sync::atomic::Ordering;
    use ::tcp::time::Instant;
    use smoltcp::wire::{EthernetFrame, EthernetProtocol, Ipv4Packet, UdpPacket};
    let _serial = SERIAL.lock().unwrap();
    let nic = Arc::new(WireDevice::new());
    let net = crate::namespace::Namespace::create().unwrap();
    let device: Arc<dyn device_model::network::NetworkDevice> = nic.clone();
    net.install_protocol(prepare_network(&net, &[device], true).unwrap());
    configure_address(&net, 2, Ipv4Addr::new(10, 0, 2, 15)).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, 2, true).unwrap();
    nic.wire.lock().unwrap().0.push_back(peer_arp_frame());
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    nic.wire.lock().unwrap().1.clear();
    let endpoint = scoped_endpoint(&net, InetProtocol::Udp);
    for id in 0u8..8 {
        endpoint.send_to(&[id], Some(udp_peer(44000)), &[]).unwrap();
    }
    assert!(!endpoint.poll_state().writable);
    nic.wire.lock().unwrap().2.submit_error = Some(device_model::network::NetworkError::WouldBlock);
    nic.transmit_ready.store(true, Ordering::Relaxed);
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    assert!(nic.wire.lock().unwrap().2.submit_error.is_none());
    assert!(!endpoint.poll_state().writable);
    assert!(nic.wire.lock().unwrap().1.is_empty());
    assert!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .udp_deadline()
            .is_none_or(|at| at > Instant::ZERO)
    );
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    assert!(
        !endpoint.poll_state().writable,
        "no busy retry without timer or device activity"
    );
    nic.transmit_ready.store(true, Ordering::Relaxed);
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    assert!(endpoint.poll_state().writable);
    let wire = nic.wire.lock().unwrap();
    let packets: Vec<_> = wire
        .1
        .iter()
        .filter_map(|frame| {
            let frame = EthernetFrame::new_checked(frame.as_slice()).ok()?;
            if frame.ethertype() != EthernetProtocol::Ipv4 {
                return None;
            }
            let ip = Ipv4Packet::new_checked(frame.payload()).ok()?;
            if ip.next_header() != smoltcp::wire::IpProtocol::Udp {
                return None;
            }
            assert!(ip.dont_frag());
            assert_eq!(ip.ident(), 0);
            let udp = UdpPacket::new_checked(ip.payload()).unwrap();
            assert!(udp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()));
            Some(udp.payload()[0])
        })
        .collect();
    assert_eq!(packets, (0u8..8).collect::<Vec<_>>());
    drop(wire);
    let drops = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .interface(2)
        .unwrap()
        .device
        .local_statistics()
        .transmitted_dropped;
    for id in 0u8..8 {
        endpoint.send_to(&[id], Some(udp_peer(44000)), &[]).unwrap();
    }
    assert!(!endpoint.poll_state().writable);
    configure_up(&net, 2, false).unwrap();
    nic.transmit_ready.store(true, Ordering::Relaxed);
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    assert!(
        endpoint.poll_state().writable,
        "lost routes must not retain the send budget forever"
    );
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        drops + 8
    );
    configure_up(&net, 2, true).unwrap();
    nic.wire.lock().unwrap().0.push_back(peer_arp_frame());
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    for id in 10u8..18 {
        endpoint.send_to(&[id], Some(udp_peer(44000)), &[]).unwrap();
    }
    nic.wire.lock().unwrap().2.submit_error = Some(device_model::network::NetworkError::Device);
    nic.transmit_ready.store(true, Ordering::Relaxed);
    let result = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(Instant::from_millis(0));
    assert!(
        result
            .device_failures
            .contains(&Some((2, device_model::network::NetworkError::Device)))
    );
    assert!(
        endpoint.poll_state().writable,
        "permanent submission errors retire that packet"
    );
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .interface(2)
            .unwrap()
            .device
            .local_statistics()
            .transmitted_dropped,
        drops + 9
    );
    endpoint.close_description();
}

// 现有夹具原先只证明收包过滤，未覆盖端口冲突与接口更改在 OOM 时的一致性。
#[test]
fn udp_and_tcp_device_bound_ports_share_without_reuse_and_rebind_is_atomic() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89245);
    for protocol in [InetProtocol::Udp, InetProtocol::Tcp] {
        let first = scoped_endpoint(&net, protocol);
        let second = scoped_endpoint(&net, protocol);
        let unbound = scoped_endpoint(&net, protocol);
        first.bind_to_device(b"eth0", false).unwrap();
        second.bind_to_device(b"eth1", false).unwrap();
        first.bind(address(43190)).unwrap();
        second.bind(address(43190)).unwrap();
        assert_eq!(unbound.bind(address(43190)), Err(SocketError::AddressInUse));
        if protocol == InetProtocol::Tcp {
            first.listen(1).unwrap();
            second.listen(1).unwrap();
        }
        crate::allocation_failure::fail_next();
        assert_eq!(
            first.bind_to_device(b"lo", true),
            Err(SocketError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(first.option(InternetOption::BoundToDevice), Ok(2));
        assert_eq!(unbound.bind_to_device(b"eth0", false), Ok(()));
        assert_eq!(unbound.bind(address(43190)), Err(SocketError::AddressInUse));
        first.bind_to_device(b"lo", true).unwrap();
        unbound.bind(address(43190)).unwrap();
        if protocol == InetProtocol::Tcp {
            unbound.listen(1).unwrap();
        }
        // 与 Linux setter 一致，改绑既有成员不重新执行 bind 准入；之后的新 bind 仍必须看到全部占用。
        first.bind_to_device(b"eth1", true).unwrap();
        first.bind_to_device(b"", true).unwrap();
        second.close_description();
        unbound.close_description();
        let probe = scoped_endpoint(&net, protocol);
        assert_eq!(probe.bind(address(43190)), Err(SocketError::AddressInUse));
        first.close_description();
        probe.bind(address(43190)).unwrap();
        probe.close_description();
    }
    let network = net.protocol().unwrap().lock().unwrap();
    assert!(network.udp_endpoints.is_empty() && network.tcp_endpoints.is_empty());
}
