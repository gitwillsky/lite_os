fn scoped_packet(
    net: &Arc<crate::namespace::Namespace>,
    protocol: u16,
) -> Arc<crate::packet::PacketSocket> {
    crate::packet::PacketSocket::new(
        network_protocol::PacketKind::Datagram,
        protocol.to_be(),
        Arc::new(crate::Event::default()), net.clone(),
    )
    .unwrap()
}
fn packet_address(index: i32, protocol: u16) -> network_protocol::PacketAddress {
    network_protocol::PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index,
        hardware_type: 1,
        packet_type: 0,
        address_length: 6,
        address: [2, 0, 0, 0, 0, 2, 0, 0],
    }
}
#[test]
fn actual_packet_socket_uses_captured_domain_link_and_native_rebinding() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let first =
        crate::namespace::Namespace::create().unwrap();
    let second =
        crate::namespace::Namespace::create().unwrap();
    let a = Arc::new(WireDevice::new());
    let b = Arc::new(WireDevice::new());
    let c = Arc::new(WireDevice::new());
    first.install_protocol(prepare_network(&first, &[a.clone()], true).unwrap());
    assert_eq!(install_interface(&first, b.clone(), crate::Channel, crate::Event::default()), Ok(3));
    second.install_protocol(prepare_network(&second, &[c.clone() as Arc<dyn device_model::network::NetworkDevice>], true).unwrap());
    let local_a = Ipv4Addr::new(10, 0, 2, 15);
    let local_b = Ipv4Addr::new(10, 0, 3, 15);
    for (net, index, address) in [
        (&first, 2, local_a),
        (&first, 3, local_b),
        (&second, 2, local_a),
    ] {
        configure_address(net, index, address).unwrap();
        configure_netmask(net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(net, index, true).unwrap();
    }
    let any_first = scoped_packet(&first, 0x0800);
    let only_a = scoped_packet(&first, 0x0800);
    only_a.bind(packet_address(2, 0x0800)).unwrap();
    let only_second = scoped_packet(&second, 0x0800);
    only_second.bind(packet_address(2, 0x0800)).unwrap();
    let disabled = scoped_packet(&first, 0);
    let first_peer = InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::new(10, 0, 3, 16), 44000));
    b.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        first_peer,
        InetAddress::V4(core::net::SocketAddrV4::new(local_b, 43217)),
        b"first-only",
    ));
    advance_actual_domains();
    assert!(any_first.poll_state().readable);
    assert!(
        !only_a.poll_state().readable
            && !only_second.poll_state().readable
            && !disabled.poll_state().readable
    );
    let mut output = [0u8; 64];
    let received = receive(&any_first, &mut output, false)
        .unwrap();
    assert_eq!(received.2.interface_index, 3);
    assert_eq!(received.2.protocol, 0x0800u16.to_be());
    assert_eq!(&output[28..received.0], b"first-only");
    // 重绑protocol0保留当前协议；显式send的出口覆盖binding只影响本次请求。
    only_a.bind(packet_address(3, 0)).unwrap();
    assert_eq!(only_a.address().unwrap().interface_index, 3);
    assert_eq!(only_a.address().unwrap().protocol, 0x0800u16.to_be());
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    only_a
        .send_to(b"explicit-link", Some(packet_address(2, 0x0806)))
        .unwrap();
    assert!(b.wire.lock().unwrap().1.is_empty());
    assert!(
        a.wire
            .lock()
            .unwrap()
            .1
            .iter()
            .any(|frame| frame[12..14] == [8, 6] && &frame[14..] == b"explicit-link")
    );
    assert_eq!(only_a.address().unwrap().interface_index, 3);
    assert_eq!(
        only_a.bind(packet_address(99, 0x0800)),
        Err(SocketError::NoDevice)
    );
    assert_eq!(
        only_a.send_to(b"missing", Some(packet_address(99, 0x0800))),
        Err(SocketError::DeviceNotAvailable)
    );
    configure_up(&first, 3, false).unwrap();
    only_a.bind(packet_address(3, 0x0800)).unwrap();
    assert!(only_a.poll_state().error);
    assert_eq!(only_a.take_error(), Some(SocketError::NetworkDown));
    assert_eq!(only_a.take_error(), None);
    assert_eq!(
        only_a.send_to(b"down", Some(packet_address(3, 0x0800))),
        Err(SocketError::NetworkDown)
    );
    crate::inet::release_interface(&first, 3).unwrap();
    assert_eq!(only_a.take_error(), Some(SocketError::NetworkDown));
    assert_eq!(only_a.bind(packet_address(3, 0)), Err(SocketError::NoDevice));
    assert_eq!(only_second.take_error(), None);
    for endpoint in [&any_first, &only_a, &only_second, &disabled] {
        endpoint.close_description();
    }
    advance_actual_domains();
}
#[test]
fn actual_packet_loopback_and_transmit_edges_stay_in_domain_and_interface() {
    use crate::packet;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let observer = scoped_packet(&net, 0x0003); // ETH_P_ALL，不依赖L3是否接受。
    observer.bind(packet_address(1, 0x0003)).unwrap();
    let sender = scoped_packet(&net, 0x0806);
    sender.bind(packet_address(1, 0x0806)).unwrap();
    assert_eq!(sender.send_to(b"non-IP-lo", None), Ok(9));
    advance_actual_domains();
    let mut output = [0u8; 32];
    let received = receive(&observer, &mut output, false)
        .unwrap();
    assert_eq!(&output[..received.0], b"non-IP-lo");
    assert_eq!(received.2.interface_index, 1);
    assert_eq!(received.2.hardware_type, 772);
    assert_eq!(received.2.protocol, 0x0806u16.to_be());
    // 清空所有actual pending，然后只发表本域lo的容量变化；initial NET不得被唤醒。
    while packet::take_pending_notification(net.packets(), 0).is_some() {}
    let initial = scoped_packet(&namespace().clone(), 0x0800);
    initial.bind(packet_address(2, 0x0800)).unwrap();
    packet::publish_transmit_ready(net.packets(), 1);
    let mut seen = 0;
    while let Some((_, endpoint)) = packet::take_pending_notification(net.packets(), 0) {
        assert!(!Arc::ptr_eq(&endpoint, &initial));
        seen += 1;
    }
    assert_eq!(seen, 2);
    assert!(packet::take_pending_notification(namespace().packets(), 0).is_none());
    for endpoint in [&observer, &sender, &initial] {
        endpoint.close_description();
    }
    advance_actual_domains();
}

#[test]
fn actual_packet_raw_preserves_link_header_and_excludes_origin_from_outgoing_tap() {
    use network_protocol::PacketKind as SocketType;
    use crate::packet;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    let nic = Arc::new(WireDevice::new());
    net.install_protocol(prepare_network(&net, &[nic.clone() as Arc<dyn device_model::network::NetworkDevice>], true).unwrap());
    configure_up(&net, 2, true).unwrap();
    let sender = packet::PacketSocket::new(
        SocketType::Raw,
        0x0800u16.to_be(),
        Arc::new(crate::Event::default()), net.clone(),
    )
    .unwrap();
    sender.bind(packet_address(2, 0x0800)).unwrap();
    let observer = scoped_packet(&net, 0x0800);
    observer.bind(packet_address(2, 0x0800)).unwrap();
    let mut frame = vec![0u8; 14];
    frame[..6].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
    frame[6..12].copy_from_slice(&[2, 0, 0, 0, 9, 9]);
    frame[12..14].copy_from_slice(&[8, 0]);
    frame.extend_from_slice(b"raw-link");
    assert_eq!(sender.send_to(&frame, None), Ok(frame.len()));
    assert_eq!(nic.wire.lock().unwrap().1.last().unwrap(), &frame);
    assert!(
        !sender.poll_state().readable,
        "native outgoing tap不得回送给发起socket"
    );
    let mut output = [0u8; 64];
    let received = receive(&observer, &mut output, false)
        .unwrap();
    assert_eq!(&output[..received.0], b"raw-link");
    assert_eq!(received.2.packet_type, 4);
    assert_eq!(&received.2.address[..6], &[2, 0, 0, 0, 9, 9]);
    // 同一实际tap的RAW projection保留header；DGRAM只剥一次，二者共享同一packet owner。
    packet::deliver(net.packets(), 2, [2, 0, 0, 0, 0, 1], &frame, false);
    let received = receive(&sender, &mut output, false)
        .unwrap();
    assert_eq!(received.1, frame.len());
    assert_eq!(&output[..received.0], frame.as_slice());
    assert_eq!(
        sender.send_to(&frame[..13], None),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        sender.send_to(&vec![0; 1515], None),
        Err(SocketError::MessageTooLarge)
    );
    assert_eq!(
        observer.send_to(&vec![0; 1501], Some(packet_address(2, 0x0800))),
        Err(SocketError::MessageTooLarge)
    );
    sender.close_description();
    observer.close_description();
    advance_actual_domains();
}
