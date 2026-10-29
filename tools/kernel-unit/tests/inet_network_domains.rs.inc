// 原样实际NET/SocketMemory/protocol/ports/cleanup/deferred；独立NIC只提供wire输入。
#[test]
fn bind_device_first_admission_and_rebind_use_actual_net_user_capability() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let socket = udp_endpoint();
    let root = crate::namespace::root_authority();
    let plain = crate::namespace::NamespaceAuthority {
        namespace: root.namespace.clone(),
        effective_uid: 1000,
        effective_gid: 1000,
        effective_capabilities: 0,
    };
    socket.bind_to_device(b"eth0", &plain).unwrap();
    assert_eq!(
        socket.option(super::super::InternetOption::BoundToDevice),
        Ok(2)
    );
    for name in [b"eth0".as_slice(), b"".as_slice()] {
        assert_eq!(
            socket.bind_to_device(name, &plain),
            Err(SocketError::OperationNotPermitted)
        );
        assert_eq!(
            socket.option(super::super::InternetOption::BoundToDevice),
            Ok(2)
        );
    }
    assert_eq!(
        socket.bind_to_device(b"missing", &plain),
        Err(SocketError::NoDevice)
    );
    let child = crate::namespace::UserNamespace::child(
        &root,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let child_admin = crate::namespace::NamespaceAuthority {
        namespace: child,
        effective_uid: 1000,
        effective_gid: 1000,
        effective_capabilities: syscall_abi::capability::Capability::NET_RAW.mask(),
    };
    assert_eq!(
        socket.bind_to_device(b"", &child_admin),
        Err(SocketError::OperationNotPermitted)
    );
    let mapped_zero_without_capability = crate::namespace::NamespaceAuthority {
        effective_uid: 0,
        effective_gid: 0,
        ..plain
    };
    assert_eq!(
        socket.bind_to_device(b"", &mapped_zero_without_capability),
        Err(SocketError::OperationNotPermitted)
    );
    socket.bind_to_device(b"", &root).unwrap();
    assert_eq!(
        socket.option(super::super::InternetOption::BoundToDevice),
        Ok(0)
    );
    socket.close_description();
}

#[test]
fn captured_network_owns_transport_ports_errors_close_and_deferred_ingress() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let first =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89201).unwrap();
    let second =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89201).unwrap();
    let a = Arc::new(crate::devices::model::WireDevice::new());
    let b = Arc::new(crate::devices::model::WireDevice::new());
    first.install_protocol(prepare_network(&first, &[a.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], false).unwrap());
    second.install_protocol(prepare_network(&second, &[b.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], false).unwrap());
    for net in [&first, &second] {
        configure_address(net, 2, Ipv4Addr::new(10, 0, 2, 15)).unwrap();
        configure_netmask(net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(net, 2, true).unwrap();
    }
    let gateway = Ipv4Addr::new(10, 0, 2, 2);
    let default = super::route::Ipv4RouteRequest {
        destination: Ipv4Addr::UNSPECIFIED, prefix_length: 0, table: 254, metric: 0,
        scope: 0, protocol: 3, kind: super::route::Ipv4RouteKind::Unicast,
        interface_index: Some(2), gateway: Some(gateway), preferred_source: None, metrics: super::route::Ipv4RouteMetrics::default(), on_link: false,
    };
    configure_route(&first, default, false, false, true, false, false).unwrap();
    assert!(route_snapshot(&first).unwrap().iter().any(|snapshot| snapshot.route == default));
    assert!(!route_snapshot(&second).unwrap().iter().any(|snapshot| snapshot.route.gateway.is_some()));
    let endpoint = |net: &Arc<crate::net::NetNamespace>, kind| {
        InetSocket::new(
            kind,
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(net.clone()).unwrap(),
        )
        .unwrap()
    };
    let udp_a = endpoint(&first, InetProtocol::Udp);
    let udp_b = endpoint(&second, InetProtocol::Udp);
    udp_a.bind(address(43200)).unwrap();
    udp_b.bind(address(43200)).unwrap();
    let duplicate = endpoint(&first, InetProtocol::Udp);
    assert!(matches!(
        duplicate.bind(address(43200)),
        Err(SocketError::AddressInUse)
    ));
    duplicate.close_description();
    // 同端口在三个域独立；初始owner不得看见非初始域slot或port membership。
    let boot = udp_endpoint();
    boot.bind(address(43200)).unwrap();
    a.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_udp_frame(udp_peer(44000), 43200, b"first"));
    b.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_udp_frame(udp_peer(44000), 43200, b"second"));
    // 使用实际目录/deferred，而非直接调用测试域的poll绕过生产推进。
    for _ in 0..4 {
        dispatch_network_work();
    }
    assert_eq!(udp_read(&udp_a, false).unwrap().0, b"first");
    assert_eq!(udp_read(&udp_b, false).unwrap().0, b"second");
    assert!(!boot.poll_state().readable);
    a.error.store(true, core::sync::atomic::Ordering::Relaxed);
    for _ in 0..4 {
        dispatch_network_work();
    }
    assert_eq!(udp_a.take_error(), None, "adapter RX failure is device-owned, not an arbitrary UDP SO_ERROR");
    assert_eq!(udp_a.take_error(), None);
    assert_eq!(udp_b.take_error(), None);
    let listener_a = endpoint(&first, InetProtocol::Tcp);
    let listener_b = endpoint(&second, InetProtocol::Tcp);
    listener_a.bind(address(43201)).unwrap();
    listener_b.bind(address(43201)).unwrap();
    listener_a.listen(1).unwrap();
    listener_b.listen(1).unwrap();
    let guard = first.protocol().unwrap().lock().unwrap();
    udp_a.close_description();
    drop(guard);
    for _ in 0..4 {
        dispatch_network_work();
    }
    let rebound = endpoint(&first, InetProtocol::Udp);
    rebound.bind(address(43200)).unwrap();
    assert_eq!(udp_b.address().unwrap(), address(43200));
    let weak = Arc::downgrade(&first);
    drop(first);
    assert!(weak.upgrade().is_some(), "socket捕获实际域，不借当前Task");
    listener_a.close_description();
    rebound.close_description();
    drop(listener_a);
    drop(rebound);
    drop(udp_a);
    drop(duplicate);
    assert!(weak.upgrade().is_none(), "Weak目录不保活最后实际NET owner");
    listener_b.close_description();
    udp_b.close_description();
    boot.close_description();
    drop(listener_b);
    drop(udp_b);
    drop(second);
}

#[test]
fn actual_protocol_preparation_oom_does_not_publish_or_copy_an_owner() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89202).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    for boundary in 0..16 {
        let device = nic.clone();
        crate::allocation_failure::fail_after(boundary);
        let owner = prepare_network(&net, &[device], false);
        if crate::allocation_failure::was_consumed() {
            assert!(owner.is_err());
        }
        drop(owner);
        assert!(net.protocol().is_none());
    }
    net.install_protocol(prepare_network(&net, &[nic], false).unwrap());
    let socket = InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(net).unwrap(),
    )
    .unwrap();
    socket.close_description();
}

fn scoped_endpoint(
    net: &Arc<crate::net::NetNamespace>,
    kind: InetProtocol,
) -> Arc<InetSocket> {
    InetSocket::new(
        kind,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(net.clone()).unwrap(),
    )
    .unwrap()
}
fn advance_actual_domains() {
    for _ in 0..16 {
        dispatch_network_work();
    }
}

#[test]
fn actual_ip_loopback_starts_down_then_delivers_udp_and_tcp_without_a_nic() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89210).unwrap();
    net.install_protocol(prepare_network(&net, &[], false).unwrap());
    let before = interface_snapshots(&net).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(&before[0].name[..3], b"lo\0");
    assert_eq!(before[0].index, 1);
    assert_eq!(before[0].address, None);
    assert!(!before[0].up);
    assert_eq!(before[0].mtu, 65536);
    assert!(matches!(
        interface_index(&net, b"eth0"),
        Err(SocketError::NoDevice)
    ));
    let udp_server = scoped_endpoint(&net, InetProtocol::Udp);
    udp_server.bind(address(43210)).unwrap();
    let udp_client = scoped_endpoint(&net, InetProtocol::Udp);
    let target = InetAddress {
        address: Ipv4Addr::LOCALHOST,
        port: 43210,
    };
    assert_eq!(
        udp_client.send_to(b"before-up", Some(target), None),
        Err(SocketError::NetworkUnreachable)
    );
    configure_up(&net, 1, true).unwrap();
    let after = interface_snapshot(&net, 1).unwrap();
    assert!(after.up && after.loopback);
    assert_eq!(after.address, Some(Ipv4Addr::LOCALHOST));
    assert_eq!(after.prefix_length, 8);
    assert_eq!(udp_client.send_to(b"loop", Some(target), None), Ok(4));
    advance_actual_domains();
    assert_eq!(udp_read(&udp_server, false).unwrap().0, b"loop");
    // 127/8由同一实际LOCAL route进入IP-medium，不能经eth0或伪Ethernet/ARP绕行。
    udp_server
        .bind_to_device(b"lo", &crate::namespace::root_authority())
        .unwrap();
    udp_client
        .send_to(
            b"prefix",
            Some(InetAddress {
                address: Ipv4Addr::new(127, 0, 0, 2),
                port: target.port,
            }),
            None,
        )
        .unwrap();
    advance_actual_domains();
    assert_eq!(udp_read(&udp_server, false).unwrap().0, b"prefix");
    let listener = scoped_endpoint(&net, InetProtocol::Tcp);
    listener
        .bind(InetAddress {
            address: Ipv4Addr::LOCALHOST,
            port: 43211,
        })
        .unwrap();
    listener.listen(1).unwrap();
    let client = scoped_endpoint(&net, InetProtocol::Tcp);
    assert_eq!(
        client.connect(InetAddress {
            address: Ipv4Addr::LOCALHOST,
            port: 43211
        }),
        Err(SocketError::InProgress)
    );
    advance_actual_domains();
    assert_eq!(client.connection_result(), Ok(()));
    let accepted = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(net.clone()).unwrap(),
        )
        .unwrap();
    assert_eq!(accepted.memory.namespace_ref().id(), net.id());
    client.send_to(b"actual TCP", None, None).unwrap();
    advance_actual_domains();
    let mut output = [0u8; 32];
    let count = accepted
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
        .unwrap()
        .0;
    assert_eq!(&output[..count], b"actual TCP");
    for endpoint in [&accepted, &client, &listener, &udp_client, &udp_server] {
        endpoint.close_description();
    }
    advance_actual_domains();
}

#[test]
fn actual_multi_nic_route_freezes_udp_oif_and_filters_bound_ingress_before_enqueue() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89211).unwrap();
    let a = Arc::new(crate::devices::model::WireDevice::new());
    let b = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[a.clone(), b.clone()], true).unwrap());
    let local_a = Ipv4Addr::new(10, 0, 2, 15);
    let local_b = Ipv4Addr::new(10, 0, 3, 15);
    for (index, address) in [(2, local_a), (3, local_b)] {
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }
    assert_eq!(interface_index(&net, b"eth0"), Ok(2));
    assert_eq!(interface_index(&net, b"eth1"), Ok(3));
    assert_eq!(interface_snapshots(&net).unwrap().len(), 3);
    let peer_b = InetAddress {
        address: Ipv4Addr::new(10, 0, 3, 16),
        port: 44000,
    };
    b.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local_b, peer_b.address));
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender.send_to(b"frozen-oif", Some(peer_b), None).unwrap();
    sender
        .bind_to_device(b"eth0", &crate::namespace::root_authority())
        .unwrap();
    // native explicit oif没有matching route时直接on-link；保留原队首冻结oif，
    // 新发送可在eth0发ARP/UDP，不能把这一标准fallback误判为ENETUNREACH。
    advance_actual_domains();
    assert!(
        a.wire.lock().unwrap().1.is_empty(),
        "队首oif不能由后续binding篡改"
    );
    assert!(b.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Udp {
            return false;
        }
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        ip.src_addr() == local_b
            && ip.dst_addr() == peer_b.address
            && udp.payload() == b"frozen-oif"
    }));
    a.wire.lock().unwrap().0.push_back(peer_arp_frame_for(local_a, peer_b.address));
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    sender.send_to(b"native-on-link", Some(peer_b), None).unwrap();
    advance_actual_domains();
    assert!(a.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 { return false; }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        ip.src_addr() == local_a && ip.dst_addr() == peer_b.address
            && UdpPacket::new_checked(ip.payload()).unwrap().payload() == b"native-on-link"
    }));
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver.bind(address(43212)).unwrap();
    receiver
        .bind_to_device(b"eth0", &crate::namespace::root_authority())
        .unwrap();
    for _ in 0..16 {
        b.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
            peer_b,
            InetAddress {
                address: local_b,
                port: 43212,
            },
            b"wrong-interface",
        ));
    }
    advance_actual_domains();
    assert!(
        !receiver.poll_state().readable,
        "wrong iface必须在receive enqueue前拒绝"
    );
    a.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 16),
            port: 44000,
        },
        InetAddress {
            address: local_a,
            port: 43212,
        },
        b"right",
    ));
    advance_actual_domains();
    assert_eq!(udp_read(&receiver, false).unwrap().0, b"right");
    // 同一实际LOCAL route使本域物理地址进入lo，source tuple不从lo地址改写。
    let local_receiver = scoped_endpoint(&net, InetProtocol::Udp);
    local_receiver
        .bind(InetAddress {
            address: local_a,
            port: 43213,
        })
        .unwrap();
    let local_sender = scoped_endpoint(&net, InetProtocol::Udp);
    local_sender
        .send_to(
            b"local-address",
            Some(InetAddress {
                address: local_a,
                port: 43213,
            }),
            None,
        )
        .unwrap();
    advance_actual_domains();
    let (bytes, source) = udp_read(&local_receiver, false).unwrap();
    assert_eq!(bytes, b"local-address");
    assert_eq!(source.unwrap().address, local_a);
    // IPv4 weak-host source在A，实际出口在B；错误iface不得在dispatch时reset TCP。
    let tcp = scoped_endpoint(&net, InetProtocol::Tcp);
    tcp.bind(InetAddress {
        address: local_a,
        port: 43214,
    })
    .unwrap();
    assert_eq!(tcp.connect(peer_b), Err(SocketError::InProgress));
    advance_actual_domains();
    assert_eq!(tcp.connection_result(), Err(SocketError::InProgress));
    assert!(b.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Tcp {
            return false;
        }
        let segment = TcpPacket::new_checked(ip.payload()).unwrap();
        ip.src_addr() == local_a
            && ip.dst_addr() == peer_b.address
            && segment.syn()
            && segment.src_port() == 43214
    }));
    for endpoint in [&tcp, &sender, &receiver, &local_receiver, &local_sender] {
        endpoint.close_description();
    }
    advance_actual_domains();
}

#[test]
fn actual_pktinfo_keeps_ingress_facts_and_overrides_only_one_transmit() {
    use crate::net::{InternetPacketInfo, InternetSendPacketInfo};
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89212).unwrap();
    let a = Arc::new(crate::devices::model::WireDevice::new());
    let b = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[a.clone(), b.clone()], true).unwrap());
    let local_a = Ipv4Addr::new(10, 0, 2, 15);
    let local_b = Ipv4Addr::new(10, 0, 3, 15);
    let peer_b = InetAddress {
        address: Ipv4Addr::new(10, 0, 3, 16),
        port: 44000,
    };
    for (index, address) in [(2, local_a), (3, local_b)] {
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }
    b.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local_b, peer_b.address));
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver.bind(address(43215)).unwrap();
    b.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        peer_b,
        InetAddress {
            address: local_b,
            port: 43215,
        },
        b"metadata",
    ));
    advance_actual_domains();
    // 已入队报文不会因后续接口地址改变而丢失iif/spec_dst，也不因peek改变。
    configure_address(&net, 3, Ipv4Addr::new(10, 0, 3, 20)).unwrap();
    let mut output = [0u8; 32];
    let expected = Some(InternetPacketInfo {
        interface_index: 3,
        specific_address: local_b,
        destination: local_b,
    });
    for peek in [true, false] {
        let received = receiver
            .receive(
                &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
                peek,
            )
            .unwrap();
        assert_eq!(received.3, expected);
        assert_eq!(&output[..received.0], b"metadata");
    }
    configure_address(&net, 3, local_b).unwrap();
    // 地址变化撤销旧 neighbor cache；重新交付真实 ARP，不假定配置恢复会恢复邻居。
    b.wire.lock().unwrap().0.push_back(peer_arp_frame_for(local_b, peer_b.address));
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender
        .bind(InetAddress {
            address: local_a,
            port: 43216,
        })
        .unwrap();
    sender
        .bind_to_device(b"eth0", &crate::namespace::root_authority())
        .unwrap();
    let override_route = Some(InternetSendPacketInfo {
        interface_index: Some(3),
        source_address: Some(local_b),
    });
    sender
        .send_to(b"one-request", Some(peer_b), override_route)
        .unwrap();
    assert_eq!(
        sender.option(super::super::InternetOption::BoundToDevice),
        Ok(2)
    );
    assert_eq!(sender.address().unwrap().address, local_a);
    assert_eq!(sender.send_to(b"original-binding", Some(peer_b), None), Ok(16));
    assert_eq!(
        sender.send_to(
            b"unknown-interface",
            Some(peer_b),
            Some(InternetSendPacketInfo {
                interface_index: Some(99),
                source_address: None
            })
        ),
        Err(SocketError::NoDevice)
    );
    assert_eq!(
        sender.send_to(
            b"nonlocal-source",
            Some(peer_b),
            Some(InternetSendPacketInfo {
                interface_index: Some(3),
                source_address: Some(Ipv4Addr::new(192, 0, 2, 1))
            })
        ),
        Err(SocketError::NetworkUnreachable)
    );
    advance_actual_domains();
    assert!(a.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        ethernet.ethertype() == EthernetProtocol::Arp
            && ArpPacket::new_checked(ethernet.payload()).unwrap().target_protocol_addr() == peer_b.address.octets()
    }), "a wire={:?}; b wire={:?}", a.wire.lock().unwrap().1, b.wire.lock().unwrap().1);
    assert!(b.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Udp {
            return false;
        }
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        ip.src_addr() == local_b && udp.src_port() == 43216 && udp.payload() == b"one-request"
    }));
    receiver.close_description();
    sender.close_description();
    advance_actual_domains();
}

#[test]
fn actual_raw_local_filter_precedes_queue_and_pktinfo_survives_reconfiguration() {
    use crate::net::InternetPacketInfo;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89213).unwrap();
    let a = Arc::new(crate::devices::model::WireDevice::new());
    let b = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[a.clone(), b.clone()], true).unwrap());
    let local_a = Ipv4Addr::new(10, 0, 2, 15);
    let local_b = Ipv4Addr::new(10, 0, 3, 15);
    for (index, address) in [(2, local_a), (3, local_b)] {
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }
    let receiver = scoped_endpoint(&net, InetProtocol::Icmp);
    receiver
        .bind(InetAddress {
            address: local_b,
            port: 0,
        })
        .unwrap();
    for _ in 0..16 {
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_icmp_frame_for(Ipv4Addr::new(10, 0, 2, 16), local_a));
    }
    advance_actual_domains();
    assert!(
        !receiver.poll_state().readable,
        "raw destination filtering必须先于enqueue，不能由recv忽略旧包"
    );
    b.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_icmp_frame_for(Ipv4Addr::new(10, 0, 3, 16), local_b));
    advance_actual_domains();
    configure_address(&net, 3, Ipv4Addr::new(10, 0, 3, 20)).unwrap();
    assert_eq!(receiver.address().unwrap().address, local_b);
    let expected = Some(InternetPacketInfo {
        interface_index: 3,
        specific_address: local_b,
        destination: local_b,
    });
    let mut output = [0u8; 64];
    for peek in [true, false] {
        let received = receiver
            .receive(
                &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
                peek,
            )
            .unwrap();
        assert_eq!(received.3, expected);
        assert_eq!(&output[16..20], &local_b.octets());
        assert_eq!(received.2.unwrap().address, Ipv4Addr::new(10, 0, 3, 16));
    }
    assert!(!receiver.poll_state().readable);
    receiver.close_description();
    advance_actual_domains();
}

fn scoped_packet(
    net: &Arc<crate::net::NetNamespace>,
    protocol: u16,
) -> Arc<super::super::packet::PacketSocket> {
    super::super::packet::PacketSocket::new(
        super::super::SocketType::Datagram,
        protocol.to_be() as usize,
        Pipe::notification_pair().unwrap(),
        crate::net::SocketMemory::try_new(net.clone()).unwrap(),
    )
    .unwrap()
}
fn packet_address(index: i32, protocol: u16) -> crate::net::PacketAddress {
    crate::net::PacketAddress {
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
    use super::super::packet;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    packet::init();
    let first =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89214).unwrap();
    let second =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89215).unwrap();
    let a = Arc::new(crate::devices::model::WireDevice::new());
    let b = Arc::new(crate::devices::model::WireDevice::new());
    let c = Arc::new(crate::devices::model::WireDevice::new());
    first.install_protocol(prepare_network(&first, &[a.clone(), b.clone()], true).unwrap());
    second.install_protocol(prepare_network(&second, &[c.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], true).unwrap());
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
    let first_peer = InetAddress {
        address: Ipv4Addr::new(10, 0, 3, 16),
        port: 44000,
    };
    b.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        first_peer,
        InetAddress {
            address: local_b,
            port: 43217,
        },
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
    let received = any_first
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
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
    for endpoint in [&any_first, &only_a, &only_second, &disabled] {
        endpoint.close_description();
    }
    advance_actual_domains();
}
#[test]
fn actual_packet_loopback_and_transmit_edges_stay_in_domain_and_interface() {
    use super::super::packet;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    packet::init();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89216).unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let observer = scoped_packet(&net, 0x0003); // ETH_P_ALL，不依赖L3是否接受。
    observer.bind(packet_address(1, 0x0003)).unwrap();
    let sender = scoped_packet(&net, 0x0806);
    sender.bind(packet_address(1, 0x0806)).unwrap();
    assert_eq!(sender.send_to(b"non-IP-lo", None), Ok(9));
    advance_actual_domains();
    let mut output = [0u8; 32];
    let received = observer
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
        .unwrap();
    assert_eq!(&output[..received.0], b"non-IP-lo");
    assert_eq!(received.2.interface_index, 1);
    assert_eq!(received.2.hardware_type, 772);
    assert_eq!(received.2.protocol, 0x0806u16.to_be());
    // 清空所有actual pending，然后只发表本域lo的容量变化；initial NET不得被唤醒。
    while packet::take_pending_notification(0).is_some() {}
    let initial = scoped_packet(&crate::net::initial_net(), 0x0800);
    initial.bind(packet_address(2, 0x0800)).unwrap();
    packet::publish_transmit_ready(net.id(), 1);
    let mut seen = 0;
    while let Some((_, endpoint)) = packet::take_pending_notification(0) {
        assert!(!Arc::ptr_eq(&endpoint, &initial));
        seen += 1;
    }
    assert_eq!(seen, 2);
    for endpoint in [&observer, &sender, &initial] {
        endpoint.close_description();
    }
    advance_actual_domains();
}

#[test]
fn actual_packet_raw_preserves_link_header_and_excludes_origin_from_outgoing_tap() {
    use super::super::{SocketType, packet};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    packet::init();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89217).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[nic.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], true).unwrap());
    configure_up(&net, 2, true).unwrap();
    let sender = packet::PacketSocket::new(
        SocketType::Raw,
        0x0800u16.to_be() as usize,
        Pipe::notification_pair().unwrap(),
        crate::net::SocketMemory::try_new(net.clone()).unwrap(),
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
    let received = observer
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
        .unwrap();
    assert_eq!(&output[..received.0], b"raw-link");
    assert_eq!(received.2.packet_type, 4);
    assert_eq!(&received.2.address[..6], &[2, 0, 0, 0, 9, 9]);
    // 同一实际tap的RAW projection保留header；DGRAM只剥一次，二者共享同一packet owner。
    packet::deliver(net.id(), 2, [2, 0, 0, 0, 0, 1], &frame);
    let received = sender
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
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
#[test]
fn actual_raw_loopback_header_survives_the_single_ip_medium_queue() {
    use super::super::{SocketType, packet};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    packet::init();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89218).unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let sender = packet::PacketSocket::new(
        SocketType::Raw,
        0x0806u16.to_be() as usize,
        Pipe::notification_pair().unwrap(),
        crate::net::SocketMemory::try_new(net.clone()).unwrap(),
    )
    .unwrap();
    sender.bind(packet_address(1, 0x0806)).unwrap();
    let observer = packet::PacketSocket::new(
        SocketType::Raw,
        0x0003u16.to_be() as usize,
        Pipe::notification_pair().unwrap(),
        crate::net::SocketMemory::try_new(net.clone()).unwrap(),
    )
    .unwrap();
    observer.bind(packet_address(1, 0x0003)).unwrap();
    let mut frame = vec![0; 14];
    frame[6..12].copy_from_slice(&[2, 0, 0, 0, 7, 7]);
    frame[12..14].copy_from_slice(&[8, 6]);
    frame.extend_from_slice(b"native-lo-header");
    assert_eq!(sender.send_to(&frame, None), Ok(frame.len()));
    let mut output = [0u8; 64];
    let outgoing = observer
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
        .unwrap();
    assert_eq!(&output[..outgoing.0], frame.as_slice());
    assert_eq!(outgoing.2.packet_type, 4);
    assert_eq!(outgoing.2.hardware_type, 772);
    assert!(!sender.poll_state().readable);
    advance_actual_domains();
    for endpoint in [&observer, &sender] {
        let incoming = endpoint
            .receive(
                &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
                false,
            )
            .unwrap();
        assert_eq!(&output[..incoming.0], frame.as_slice());
        assert_eq!(incoming.2.packet_type, 0);
        assert_eq!(incoming.2.hardware_type, 772);
    }
    sender.close_description();
    observer.close_description();
    advance_actual_domains();
}

#[test]
fn actual_rx_and_tcp_ack_do_not_wait_for_hardware_tx_reservation() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89219).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[nic.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], true).unwrap());
    let local = Ipv4Addr::new(10, 0, 2, 15);
    configure_address(&net, 2, local).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, 2, true).unwrap();
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver.bind(address(43218)).unwrap();
    nic.transmit_available
        .store(false, core::sync::atomic::Ordering::Relaxed);
    let peer = InetAddress {
        address: Ipv4Addr::new(10, 0, 2, 16),
        port: 44000,
    };
    nic.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        peer,
        InetAddress {
            address: local,
            port: 43218,
        },
        b"rx-with-full-tx",
    ));
    advance_actual_domains();
    assert_eq!(udp_read(&receiver, false).unwrap().0, b"rx-with-full-tx");
    // 同一RxToken遇到必须回复ARP时可丢reply并继续消费输入，不把WouldBlock提升成EIO。
    nic.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local, peer.address));
    advance_actual_domains();
    assert!(nic.wire.lock().unwrap().0.is_empty());
    let network = net.protocol().unwrap().lock().unwrap();
    assert!(
        network.interfaces[1]
            .device
            .statistics()
            .transmitted_dropped
            > 0
    );
    assert!(network.interfaces[1].device.take_error().is_none());
    drop(network);
    nic.transmit_available
        .store(true, core::sync::atomic::Ordering::Relaxed);
    receiver.send_to(b"resumed", Some(peer), None).unwrap();
    advance_actual_domains();
    assert!(nic.wire.lock().unwrap().1.iter().any(|frame| {
        let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        ip.next_header() == IpProtocol::Udp
            && UdpPacket::new_checked(ip.payload()).unwrap().payload() == b"resumed"
    }));
    let listener = scoped_endpoint(&net, InetProtocol::Tcp);
    listener.bind(address(43219)).unwrap();
    listener.listen(1).unwrap();
    nic.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_tcp_frame(43219, TcpControl::Syn, None));
    advance_actual_domains();
    let ack = nic
        .wire
        .lock()
        .unwrap()
        .1
        .iter()
        .find_map(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).ok()?;
            if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                return None;
            }
            let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
            if ip.next_header() != IpProtocol::Tcp {
                return None;
            }
            let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
            (tcp.src_port() == 43219 && tcp.syn() && tcp.ack()).then(|| tcp.seq_number() + 1)
        })
        .expect("实际producer必须先发SYN/ACK");
    nic.transmit_available
        .store(false, core::sync::atomic::Ordering::Relaxed);
    nic.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_tcp_frame(43219, TcpControl::None, Some(ack)));
    advance_actual_domains();
    let child = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(net.clone()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        child.connection_result(),
        Ok(()),
        "TX满不能阻止实际final ACK建立连接"
    );
    child.close_description();
    listener.close_description();
    nic.transmit_available
        .store(true, core::sync::atomic::Ordering::Relaxed);
    receiver.close_description();
    advance_actual_domains();
}

#[test]
fn actual_network_snapshot_contains_every_link_and_keeps_domains_separate() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let first =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89220).unwrap();
    let second =
        crate::net::NetNamespace::create(crate::namespace::initial_user(), 89220).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    first.install_protocol(
        prepare_network(
            &first,
            &[nic.clone(), Arc::new(crate::devices::model::WireDevice::new())],
            true,
        )
        .unwrap(),
    );
    second.install_protocol(prepare_network(&second, &[], false).unwrap());
    configure_address(&first, 3, Ipv4Addr::new(10, 8, 0, 15)).unwrap();
    configure_netmask(&first, 3, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&first, 3, true).unwrap();
    let rows = network_snapshot(&first).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(&rows[0].name[..3], b"lo\0");
    let configured = interface_snapshots(&first).unwrap();
    assert!(configured[0].loopback && configured[0].up);
    assert_eq!(&rows[1].name[..5], b"eth0\0");
    assert_eq!(&rows[2].name[..5], b"eth1\0");
    assert_eq!(configured[2].address, Some(Ipv4Addr::new(10, 8, 0, 15)));
    configure_address(&first, 3, Ipv4Addr::new(10, 9, 0, 15)).unwrap();
    assert_eq!(
        configured[2].address,
        Some(Ipv4Addr::new(10, 8, 0, 15)),
        "lock-external scalar snapshot is immutable"
    );
    assert_eq!(
        interface_snapshot(&first, 3).unwrap().address,
        Some(Ipv4Addr::new(10, 9, 0, 15))
    );
    let other = network_snapshot(&second).unwrap();
    assert_eq!(other.len(), 1);
    let isolated = interface_snapshot(&second, 1).unwrap();
    assert!(isolated.loopback && !isolated.up && isolated.address.is_none());
}

#[test]
fn actual_ipv4_address_record_uses_native_classful_mask_and_explicit_broadcast() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89221).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[nic.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], true).unwrap());
    assert_eq!(
        configure_netmask(&net, 2, Ipv4Addr::new(255, 0, 0, 0)),
        Err(SocketError::AddressNotAvailable)
    );
    assert_eq!(
        configure_broadcast(&net, 2, Ipv4Addr::BROADCAST),
        Err(SocketError::AddressNotAvailable)
    );
    for (local, prefix) in [
        (Ipv4Addr::new(10, 2, 3, 15), 8),
        (Ipv4Addr::new(172, 16, 3, 15), 16),
        (Ipv4Addr::new(192, 168, 3, 15), 24),
        (Ipv4Addr::new(240, 1, 1, 1), 32),
        (Ipv4Addr::new(0, 1, 1, 1), 0),
        (Ipv4Addr::BROADCAST, 0),
    ] {
        configure_address(&net, 2, local).unwrap();
        assert_eq!(interface_snapshot(&net, 2).unwrap().prefix_length, prefix);
    }
    assert_eq!(
        configure_address(&net, 2, Ipv4Addr::new(224, 0, 0, 1)),
        Err(SocketError::Invalid)
    );
    let local = Ipv4Addr::new(10, 8, 3, 15);
    configure_address(&net, 2, local).unwrap();
    configure_up(&net, 2, true).unwrap();
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().broadcast,
        Ipv4Addr::new(10, 255, 255, 255)
    );
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().broadcast,
        Ipv4Addr::new(10, 8, 3, 255)
    );
    let explicit = Ipv4Addr::new(10, 8, 3, 99);
    configure_broadcast(&net, 2, explicit).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 0, 0)).unwrap();
    configure_address(&net, 2, local).unwrap();
    let configured = interface_snapshot(&net, 2).unwrap();
    assert_eq!(
        configured.prefix_length, 16,
        "same address keeps configured mask"
    );
    assert_eq!(
        configured.broadcast, explicit,
        "funny broadcast is the actual record, not a prefix guess"
    );
    assert_eq!(
        interface_snapshot(&net, 1).unwrap().broadcast,
        Ipv4Addr::UNSPECIFIED
    );
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver
        .bind(InetAddress {
            address: local,
            port: 43221,
        })
        .unwrap();
    let peer = InetAddress {
        address: Ipv4Addr::new(10, 8, 3, 16),
        port: 44000,
    };
    nic.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        peer,
        InetAddress {
            address: explicit,
            port: 43221,
        },
        b"native configured broadcast",
    ));
    advance_actual_domains();
    let mut bytes = [0; 64];
    let count = receiver
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
        )
        .unwrap()
        .0;
    assert_eq!(&bytes[..count], b"native configured broadcast");
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    assert_eq!(
        sender.send_to(
            b"x",
            Some(InetAddress {
                address: explicit,
                port: 43221
            }),
            None
        ),
        Err(SocketError::PermissionDenied)
    );
    sender.set_broadcast(true).unwrap();
    sender
        .send_to(
            b"x",
            Some(InetAddress {
                address: explicit,
                port: 43221,
            }),
            None,
        )
        .unwrap();
    advance_actual_domains();
    assert!(
        nic.wire
            .lock()
            .unwrap()
            .1
            .iter()
            .any(|frame| frame.starts_with(&[255; 6])),
        "actual configured broadcast must select Ethernet broadcast without ARP"
    );
    configure_address(&net, 2, Ipv4Addr::new(10, 8, 3, 17)).unwrap();
    assert_eq!(interface_snapshot(&net, 2).unwrap().prefix_length, 8);
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().broadcast,
        Ipv4Addr::new(10, 255, 255, 255)
    );
    receiver.close_description();
    sender.close_description();
    configure_address(&net, 2, Ipv4Addr::UNSPECIFIED).unwrap();
    assert_eq!(interface_snapshot(&net, 2).unwrap().address, None);
}

#[test]
fn native_newnet_factory_prepares_real_empty_stack_and_oom_releases_quota() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let root = crate::namespace::root_authority();
    let user = crate::namespace::UserNamespace::child(
        &root,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    user.set_resource_limit(crate::namespace::UserResource::Net, 1, &root)
        .unwrap();
    for boundary in 0..32 {
        crate::allocation_failure::fail_after(boundary);
        let prepared = create_net_namespace(user.clone(), 89222);
        if crate::allocation_failure::was_consumed() {
            assert!(matches!(
                prepared,
                // Linux inc_net_namespaces 的 ucounts allocation 失败与配额耗尽同为 ENOSPC；
                // 后续 net/transport owner allocation 才是 ENOMEM。
                Err(crate::namespace::NamespaceError::OutOfMemory | crate::namespace::NamespaceError::QuotaLimit)
            ));
        }
        drop(prepared);
        let next = create_net_namespace(user.clone(), 89222).unwrap();
        let interfaces = interface_snapshots(&next).unwrap();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(&interfaces[0].name[..3], b"lo\0");
        assert!(interfaces[0].loopback && !interfaces[0].up && interfaces[0].address.is_none());
        let namespace = crate::namespace::NamespaceReference::new(next.clone());
        assert_eq!(namespace.kind(), crate::namespace::NamespaceType::Net);
        assert_eq!(namespace.kind().flag(), 0x4000_0000);
        assert!(Arc::ptr_eq(&namespace.owning_user().unwrap(), &user));
        assert!(Arc::ptr_eq(
            &namespace.downcast::<crate::net::NetNamespace>().unwrap(),
            &next
        ));
        assert!(matches!(
            namespace.parent_reference(),
            Err(crate::namespace::NamespaceError::Invalid)
        ));
        assert!(matches!(
            create_net_namespace(user.clone(), 89222),
            Err(crate::namespace::NamespaceError::QuotaLimit)
        ));
        drop(next);
        assert!(
            matches!(
                create_net_namespace(user.clone(), 89222),
                Err(crate::namespace::NamespaceError::QuotaLimit)
            ),
            "nsfs/socket observer keeps the actual domain alive"
        );
        drop(namespace);
    }
    let net = create_net_namespace(user, 89222).unwrap();
    configure_up(&net, 1, true).unwrap();
    let server = scoped_endpoint(&net, InetProtocol::Udp);
    let address = InetAddress {
        address: Ipv4Addr::LOCALHOST,
        port: 43222,
    };
    server.bind(address).unwrap();
    let client = scoped_endpoint(&net, InetProtocol::Udp);
    client
        .send_to(
            b"native empty domain becomes actual loopback",
            Some(address),
            None,
        )
        .unwrap();
    advance_actual_domains();
    let mut bytes = [0; 64];
    let count = server
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut bytes),
            false,
        )
        .unwrap()
        .0;
    assert_eq!(
        &bytes[..count],
        b"native empty domain becomes actual loopback"
    );
    client.close_description();
    server.close_description();
}

#[test]
fn actual_interface_mtu_changes_packet_admission_and_protocol_segment_mechanism() {
    use smoltcp::wire::{EthernetFrame, EthernetProtocol, Ipv4Packet, IpProtocol};
    use super::super::packet;
    use smoltcp::phy::Device;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    packet::init();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89233).unwrap();
    let first = Arc::new(crate::devices::model::WireDevice::new());
    let second = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[first.clone(), second.clone()], true).unwrap());
    for (index, address) in [
        (2, Ipv4Addr::new(10, 0, 2, 15)),
        (3, Ipv4Addr::new(10, 0, 3, 15)),
    ] {
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }
    for mtu in [-1, 0, 67, 1501, i32::MAX] {
        assert_eq!(configure_mtu(&net, 2, mtu), Err(SocketError::Invalid));
        assert_eq!(interface_snapshot(&net, 2).unwrap().mtu, 1500);
    }
    configure_mtu(&net, 2, 256).unwrap();
    configure_mtu(&net, 3, 1000).unwrap();
    assert_eq!(interface_snapshot(&net, 2).unwrap().mtu, 256);
    assert_eq!(interface_snapshot(&net, 3).unwrap().mtu, 1000);
    let stack = net.protocol().unwrap().lock().unwrap();
    for (index, mtu) in [(2, 256), (3, 1000)] {
        let interface = stack.interface(index).unwrap();
        assert_eq!(interface.device.capabilities().ip_mtu(), mtu);
    }
    drop(stack);
    // 从真实SYN的MSS观察cached协议caps，而非增加只供测试读取的library getter。
    for (device, local, peer, mss) in [
        (
            &first,
            Ipv4Addr::new(10, 0, 2, 15),
            Ipv4Addr::new(10, 0, 2, 16),
            256,
        ),
        (
            &second,
            Ipv4Addr::new(10, 0, 3, 15),
            Ipv4Addr::new(10, 0, 3, 16),
            960,
        ),
    ] {
        device
            .wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, peer));
        advance_actual_domains();
        device.wire.lock().unwrap().1.clear();
        let client = scoped_endpoint(&net, InetProtocol::Tcp);
        assert_eq!(
            client.connect(InetAddress {
                address: peer,
                port: 44000
            }),
            Err(SocketError::InProgress)
        );
        advance_actual_domains();
        assert!(device.wire.lock().unwrap().1.iter().any(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                return false;
            }
            let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp {
                return false;
            }
            let tcp = smoltcp::wire::TcpPacket::new_checked(ip.payload()).unwrap();
            tcp.syn()
                && smoltcp::wire::TcpRepr::parse(
                    &tcp,
                    &ipv4(local),
                    &ipv4(peer),
                    &smoltcp::phy::ChecksumCapabilities::default(),
                )
                .unwrap()
                .max_seg_size
                    == Some(mss)
        }));
        client.close_description();
        advance_actual_domains();
        device.wire.lock().unwrap().1.clear();
    }
    let sender = scoped_packet(&net, 0x0806);
    sender.bind(packet_address(2, 0x0806)).unwrap();
    assert_eq!(
        sender.send_to(&[0x41; 257], None),
        Err(SocketError::MessageTooLarge)
    );
    assert!(first.wire.lock().unwrap().1.is_empty());
    assert_eq!(sender.send_to(&[0x42; 256], Some(packet_address(2, 0x0806))), Ok(256));
    assert_eq!(first.wire.lock().unwrap().1.last().unwrap().len(), 270);
    configure_mtu(&net, 2, 128).unwrap();
    assert_eq!(
        sender.send_to(&[0x41; 129], None),
        Err(SocketError::MessageTooLarge)
    );
    assert_eq!(
        sender.send_to(&[0x43; 900], Some(packet_address(3, 0x0806))),
        Ok(900)
    );
    assert_eq!(second.wire.lock().unwrap().1.last().unwrap().len(), 914);
    sender.close_description();
    assert_eq!(
        interface_snapshot(&crate::net::initial_net(), 2)
            .unwrap()
            .mtu,
        1500
    );
}
#[test]
fn native_lo_small_mtu_retires_ipv4_and_restore_does_not_recreate_old_address() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = create_net_namespace(crate::namespace::initial_user(), 89234).unwrap();
    configure_up(&net, 1, true).unwrap();
    for invalid in [-1, 65537, i32::MAX] {
        assert_eq!(configure_mtu(&net, 1, invalid), Err(SocketError::Invalid));
    }
    configure_mtu(&net, 1, 512).unwrap();
    let server = scoped_endpoint(&net, InetProtocol::Udp);
    let target = InetAddress {
        address: Ipv4Addr::LOCALHOST,
        port: 43223,
    };
    server.bind(target).unwrap();
    server.send_to(&[0x45; 128], Some(target), None).unwrap();
    advance_actual_domains();
    assert_eq!(udp_read(&server, false).unwrap().0, [0x45; 128]);
    configure_mtu(&net, 1, 0).unwrap();
    let snapshot = interface_snapshot(&net, 1).unwrap();
    assert_eq!(snapshot.mtu, 0);
    assert_eq!(snapshot.address, None);
    assert!(snapshot.up);
    assert_eq!(
        configure_address(&net, 1, Ipv4Addr::LOCALHOST),
        Err(SocketError::NoBuffer)
    );
    assert_eq!(
        server.send_to(b"gone", Some(target), None),
        Err(SocketError::NetworkUnreachable)
    );
    configure_mtu(&net, 1, 65536).unwrap();
    assert_eq!(interface_snapshot(&net, 1).unwrap().address, None);
    configure_up(&net, 1, false).unwrap();
    configure_up(&net, 1, true).unwrap();
    assert_eq!(
        interface_snapshot(&net, 1).unwrap().address,
        Some(Ipv4Addr::LOCALHOST)
    );
    server.send_to(b"restored", Some(target), None).unwrap();
    advance_actual_domains();
    assert_eq!(udp_read(&server, false).unwrap().0, b"restored");
    server.close_description();
    advance_actual_domains();
}

#[test]
fn actual_nic_failure_does_not_poison_socket_errors_or_erase_other_link_completion_and_backlog() {
    use core::sync::atomic::Ordering::Relaxed;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89244);
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver.bind(address(43288)).unwrap();
    receiver.bind_to_device(b"eth1", &crate::namespace::root_authority()).unwrap();
    let idle = scoped_endpoint(&net, InetProtocol::Tcp);
    a.error.store(true, Relaxed);
    b.transmit_ready.store(true, Relaxed);
    for _ in 0..=NETWORK_RX_BUDGET {
        b.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
            InetAddress { address: Ipv4Addr::new(10, 41, 0, 16), port: 44000 },
            InetAddress { address: Ipv4Addr::new(10, 41, 0, 15), port: 43288 }, b"b",
        ));
    }
    let outcome = net.protocol().unwrap().lock().unwrap().poll();
    assert_eq!(outcome.device_failures.into_iter().flatten().collect::<Vec<_>>(),
        [(2, crate::devices::model::network::NetworkError::Device)]);
    assert!(outcome.packet_transmit_ready.contains(&Some(3)), "NIC A error must not erase NIC B completion");
    assert!(outcome.backlog, "NIC A error must not discard exhausted RX budget");
    assert_eq!(b.wire.lock().unwrap().0.len(), 1);
    assert_eq!(receiver.take_error(), None);
    assert_eq!(idle.take_error(), None);
    assert!(!receiver.poll_state().error && !idle.poll_state().error);
    assert_eq!(udp_read(&receiver, false).unwrap().0, b"b");
    let snapshots = network_snapshot(&net).unwrap();
    assert_eq!(snapshots[1].statistics.received_errors, 1);
    assert_eq!(snapshots[2].statistics.received_errors, 0);
    let outcome = net.protocol().unwrap().lock().unwrap().poll();
    assert!(outcome.device_failures.into_iter().all(|failure| failure.is_none()), "actual diagnostic is consumed once per error event");
    receiver.close_description(); idle.close_description();
}

#[test]
fn actual_raw255_header_codec_preserves_wire_and_send_only_admission() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89301).unwrap();
    net.install_protocol(prepare_network(&net, &[], false).unwrap());
    configure_up(&net, 1, true).unwrap();
    let sender = scoped_endpoint(&net, InetProtocol::Raw);
    let receiver = scoped_endpoint(&net, InetProtocol::Icmp);
    let peer = InetAddress { address: Ipv4Addr::LOCALHOST, port: 123 };
    assert_eq!(sender.option(super::super::InternetOption::HeaderIncluded), Ok(1));
    assert_eq!(receiver.option(super::super::InternetOption::HeaderIncluded), Ok(0));
    let mut packet = [0x45, 0x10, 0xff, 0xff, 0x12, 0x34, 0, 0, 37, 1, 0xa5, 0xa5,
        0, 0, 0, 0, 127, 0, 0, 1, 8, 0, 0xf7, 0xff, 0, 0, 0, 0];
    for generated in [false, true] {
        if generated { packet[4..6].fill(0); }
        let original = packet;
        assert_eq!(sender.send_to(&packet, Some(peer), None), Ok(packet.len()));
        // 冻结后的队列包含真实完整 header；后续 option 不得改变旧 packet。
        sender.set_header_included(false).unwrap();
        advance_actual_domains();
        let mut output = [0u8; 64];
        let received = receiver.receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut output), false).unwrap();
        assert_eq!(received.0, packet.len());
        assert_eq!(packet, original);
        let header = smoltcp::wire::Ipv4Packet::new_checked(&output[..received.0]).unwrap();
        assert!(header.verify_checksum());
        assert_eq!(header.total_len() as usize, packet.len());
        assert_eq!(&output[..2], &packet[..2]);
        assert_eq!(&output[6..10], &packet[6..10]);
        assert_eq!(&output[12..16], &Ipv4Addr::LOCALHOST.octets());
        assert_eq!(&output[16..28], &packet[16..]);
        if generated { assert_ne!(&output[4..6], &[0, 0]); }
        else { assert_eq!(&output[4..6], &packet[4..6]); }
        assert!(!sender.poll_state().readable, "RAW255 不以 wildcard RX 冒充 send-only");
        // ICMP echo reply 可能也在队列中；读取完毕再发布下一次请求。
        while receiver.poll_state().readable {
            receiver.receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut output), false).unwrap();
        }
        sender.set_header_included(true).unwrap();
    }
    // 准入失败既不修改输入，也不污染实际 transport queue。
    for (offset, value, error) in [(0, 0x46, SocketError::OperationNotSupported),
        (6, 0x20, SocketError::OperationNotSupported), (0, 0x65, SocketError::Invalid),
        (19, 2, SocketError::OperationNotSupported)] {
        let mut invalid = packet; invalid[offset] = value;
        assert_eq!(sender.send_to(&invalid, Some(peer), None), Err(error));
    }
    assert_eq!(sender.send_to(&packet[..19], Some(peer), None), Err(SocketError::Invalid));
    advance_actual_domains();
    assert!(!receiver.poll_state().readable);
    sender.close_description(); receiver.close_description();
    advance_actual_domains();
}

#[test]
fn actual_raw_header_preserves_explicit_source_id_df_and_native_protocol() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::net::NetNamespace::create(crate::namespace::initial_user(), 89302).unwrap();
    let nic = Arc::new(crate::devices::model::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[nic.clone() as Arc<dyn crate::devices::model::network::NetworkDevice>], true).unwrap());
    let local = Ipv4Addr::new(10, 0, 2, 15);
    let peer = InetAddress { address: Ipv4Addr::new(10, 0, 2, 16), port: 0 };
    configure_address(&net, 2, local).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, 2, true).unwrap();
    nic.wire.lock().unwrap().0.push_back(peer_arp_frame_for(local, peer.address));
    advance_actual_domains();
    nic.wire.lock().unwrap().1.clear();
    let sender = scoped_endpoint(&net, InetProtocol::Icmp);
    sender.set_header_included(true).unwrap();
    let mut packet = [0x45, 0x14, 0, 0, 0xab, 0xcd, 0x40, 0, 19, 253, 0, 0,
        192, 0, 2, 1, 10, 0, 2, 16, 1, 2, 3, 4];
    assert_eq!(sender.send_to(&packet, Some(peer), None), Ok(24));
    sender.set_header_included(false).unwrap();
    advance_actual_domains();
    let frames = nic.wire.lock().unwrap();
    let wire = frames.1.iter().find_map(|bytes| {
        let eth = smoltcp::wire::EthernetFrame::new_checked(bytes.as_slice()).ok()?;
        (eth.ethertype() == smoltcp::wire::EthernetProtocol::Ipv4).then_some(bytes)
    }).unwrap();
    let eth = smoltcp::wire::EthernetFrame::new_checked(wire.as_slice()).unwrap();
    let ip = smoltcp::wire::Ipv4Packet::new_checked(eth.payload()).unwrap();
    assert!(ip.verify_checksum());
    packet[2..4].copy_from_slice(&24u16.to_be_bytes());
    packet[10..12].copy_from_slice(&eth.payload()[10..12]);
    assert_eq!(&eth.payload()[..24], &packet);
    drop(frames);
    // 普通 codec 使用 endpoint 的 protocol，不从上一条用户 header 继承。
    assert_eq!(sender.send_to(&[8, 0, 0xf7, 0xff, 0, 0, 0, 0], Some(peer), None), Ok(8));
    advance_actual_domains();
    assert!(nic.wire.lock().unwrap().1.iter().any(|frame| {
        let eth = smoltcp::wire::EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if eth.ethertype() != smoltcp::wire::EthernetProtocol::Ipv4 { return false; }
        let ip = smoltcp::wire::Ipv4Packet::new_checked(eth.payload()).unwrap();
        ip.next_header() == smoltcp::wire::IpProtocol::Icmp && ip.src_addr() == local && ip.verify_checksum()
    }));
    sender.close_description(); advance_actual_domains();
}
