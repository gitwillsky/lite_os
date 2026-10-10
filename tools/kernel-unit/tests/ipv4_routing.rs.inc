// 固定Linux FIB/route_output语义；actual NET/interface/socket/neighbor全部载入生产路径。
fn routing_domain(
    uid: u32,
) -> (
    Arc<crate::socket::NetNamespace>,
    Arc<crate::drivers::WireDevice>,
    Arc<crate::drivers::WireDevice>,
) {
    let net = crate::socket::NetNamespace::create(crate::namespace::initial_user(), uid).unwrap();
    let a = Arc::new(crate::drivers::WireDevice::new());
    let b = Arc::new(crate::drivers::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[a.clone(), b.clone()], true).unwrap());
    for (index, address) in [
        (2, Ipv4Addr::new(10, 40, 0, 15)),
        (3, Ipv4Addr::new(10, 41, 0, 15)),
    ] {
        configure_address(&net, index, address).unwrap();
        configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, index, true).unwrap();
    }
    (net, a, b)
}
fn static_route(
    destination: Ipv4Addr,
    prefix_length: u8,
    index: u32,
    metric: u32,
) -> super::route::Ipv4RouteRequest {
    super::route::Ipv4RouteRequest {
        destination,
        prefix_length,
        table: 254,
        metric,
        scope: 253,
        protocol: 3,
        kind: super::route::Ipv4RouteKind::Unicast,
        interface_index: Some(index),
        gateway: None,
        preferred_source: None,
        metrics: super::route::Ipv4RouteMetrics::default(),
        on_link: false,
    }
}
#[test]
fn actual_fib_longest_prefix_metric_alias_order_and_exact_replace_are_native() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89231);
    let wide = static_route(Ipv4Addr::new(172, 16, 0, 0), 16, 2, 1);
    let narrow = static_route(Ipv4Addr::new(172, 16, 8, 0), 24, 3, 90);
    for route in [wide, narrow] {
        configure_route(&net, route, false, false, true, false, false).unwrap();
    }
    let destination = Ipv4Addr::new(172, 16, 8, 9);
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(destination, None)
            .unwrap()
            .interface_index,
        3
    );
    let low = super::route::Ipv4RouteRequest {
        metric: 3,
        interface_index: Some(2),
        ..narrow
    };
    configure_route(&net, low, false, false, true, false, false).unwrap();
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(destination, None)
            .unwrap()
            .interface_index,
        2
    );
    let alias = super::route::Ipv4RouteRequest {
        interface_index: Some(3),
        ..low
    };
    configure_route(&net, alias, false, false, true, false, false).unwrap();
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(destination, None)
            .unwrap()
            .interface_index,
        3
    );
    assert_eq!(
        configure_route(&net, alias, false, false, true, false, false),
        Err(super::route::Ipv4RouteError::Exists)
    );
    crate::allocation_failure::fail_next();
    assert_eq!(
        configure_route(&net, alias, false, true, false, false, false),
        Ok(())
    );
    assert!(
        !crate::allocation_failure::was_consumed(),
        "exact first alias replacement must be allocation-free"
    );
    configure_route(&net, alias, true, false, false, false, false).unwrap();
    configure_route(&net, alias, false, false, true, false, true).unwrap();
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(destination, None)
            .unwrap()
            .interface_index,
        2
    );
    assert_eq!(
        configure_route(&net, alias, false, true, true, true, false),
        Err(super::route::Ipv4RouteError::Exists)
    );
    for route in [low, alias, narrow, wide] {
        configure_route(&net, route, true, false, false, false, false).unwrap();
    }
    assert_eq!(
        configure_route(&net, wide, true, false, false, false, false),
        Err(super::route::Ipv4RouteError::NotFound)
    );
    assert_eq!(
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route(destination, None),
        Err(SocketError::NetworkUnreachable)
    );
}
#[test]
fn actual_fib_gateway_admission_and_distinct_reject_errors_do_not_modify_existing_routes() {
    use super::route::{Ipv4RouteError as E, Ipv4RouteKind as K, Ipv4RouteRequest};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89232);
    let default = Ipv4RouteRequest {
        gateway: Some(Ipv4Addr::new(10, 40, 0, 1)),
        scope: 0,
        ..static_route(Ipv4Addr::UNSPECIFIED, 0, 2, 1)
    };
    configure_route(&net, default, false, false, true, false, false).unwrap();
    let unreachable_gateway = Ipv4RouteRequest {
        gateway: Some(Ipv4Addr::new(198, 51, 100, 1)),
        interface_index: None,
        ..static_route(Ipv4Addr::new(203, 0, 113, 0), 24, 2, 1)
    };
    let unreachable_gateway = Ipv4RouteRequest {
        scope: 0,
        ..unreachable_gateway
    };
    assert_eq!(
        configure_route(&net, unreachable_gateway, false, false, true, false, false),
        Err(E::NetworkUnreachable)
    );
    let directly_reachable = static_route(Ipv4Addr::new(198, 51, 100, 0), 24, 3, 1);
    configure_route(&net, directly_reachable, false, false, true, false, false).unwrap();
    configure_route(&net, unreachable_gateway, false, false, true, false, false).unwrap();
    let selected = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .route(Ipv4Addr::new(203, 0, 113, 9), None)
        .unwrap();
    assert_eq!(
        (selected.interface_index, selected.next_hop),
        (3, Ipv4Addr::new(198, 51, 100, 1))
    );
    {
        let (kind, error) = (K::Unreachable, SocketError::HostUnreachable);
        let reject = Ipv4RouteRequest {
            kind,
            scope: 254,
            interface_index: None,
            ..static_route(Ipv4Addr::new(203, 0, 113, 9), 32, 2, 0)
        };
        configure_route(&net, reject, false, false, true, false, false).unwrap();
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(reject.destination, None),
            Err(error)
        );
        assert_eq!(
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .route(reject.destination, Some(2))
                .unwrap()
                .next_hop,
            Ipv4Addr::new(10, 40, 0, 1)
        );
        configure_route(&net, reject, true, false, false, false, false).unwrap();
    }
    let nonlocal = Ipv4RouteRequest {
        preferred_source: Some(Ipv4Addr::new(192, 0, 2, 1)),
        ..directly_reachable
    };
    assert_eq!(
        configure_route(&net, nonlocal, false, true, true, false, false),
        Err(E::Invalid)
    );
    let misaligned = Ipv4RouteRequest {
        destination: Ipv4Addr::new(198, 51, 100, 1),
        ..directly_reachable
    };
    assert_eq!(
        configure_route(&net, misaligned, false, false, true, false, false),
        Err(E::Invalid)
    );
}
#[test]
fn actual_address_and_fib_publication_roll_back_every_preparation_failure() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89233);
    let old = Ipv4Addr::new(10, 40, 0, 15);
    for fault in 0..7 {
        configure_address(&net, 2, old).unwrap();
        configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        let manual = static_route(Ipv4Addr::new(172, 20, 0, 0), 16, 2, 12);
        if !route_snapshot(&net)
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.route == manual)
        {
            configure_route(&net, manual, false, false, true, false, false).unwrap();
        }
        let before = route_snapshot(&net)
            .unwrap()
            .into_iter()
            .map(|snapshot| snapshot.route)
            .collect::<Vec<_>>();
        crate::allocation_failure::fail_after(fault);
        let changed = configure_address(&net, 2, Ipv4Addr::new(10, 40, 1, 15));
        if crate::allocation_failure::was_consumed() {
            assert_eq!(changed, Err(SocketError::NoMemory));
            let actual = interface_snapshot(&net, 2).unwrap();
            assert_eq!(
                (actual.address, actual.prefix_length, actual.broadcast),
                (Some(old), 24, Ipv4Addr::new(10, 40, 0, 255))
            );
            assert_eq!(
                route_snapshot(&net)
                    .unwrap()
                    .into_iter()
                    .map(|snapshot| snapshot.route)
                    .collect::<Vec<_>>(),
                before
            );
        } else {
            changed.unwrap();
        }
    }
}
#[test]
fn actual_down_local_delivery_retains_host_route_but_up_never_revives_old_static_route() {
    use super::route::Ipv4RouteKind;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89234);
    let local = Ipv4Addr::new(10, 40, 0, 15);
    let foreign = static_route(Ipv4Addr::new(172, 21, 0, 0), 16, 2, 0);
    configure_route(&net, foreign, false, false, true, false, false).unwrap();
    configure_up(&net, 2, false).unwrap();
    let selected = net
        .protocol()
        .unwrap()
        .lock()
        .unwrap()
        .route(local, None)
        .unwrap();
    assert_eq!(
        (
            selected.interface_index,
            selected.logical_interface_index,
            selected.kind
        ),
        (1, 2, Ipv4RouteKind::Local)
    );
    assert!(
        !route_snapshot(&net)
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.route == foreign)
    );
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver
        .bind(InetAddress {
            address: local,
            port: 43231,
        })
        .unwrap();
    receiver
        .bind_to_device(b"eth0", &crate::namespace::root_authority())
        .unwrap();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender
        .send_to(
            b"down-local",
            Some(InetAddress {
                address: local,
                port: 43231,
            }),
            None,
        )
        .unwrap();
    advance_actual_domains();
    let mut output = [0; 32];
    let (count, _, _, info) = receiver
        .receive(
            &mut crate::ipc::ReceiveBuffer::from_slice(&mut output),
            false,
        )
        .unwrap();
    assert_eq!(&output[..count], b"down-local");
    assert_eq!(info.unwrap().interface_index, 2);
    configure_up(&net, 2, true).unwrap();
    assert!(
        !route_snapshot(&net)
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.route == foreign)
    );
    configure_address(&net, 2, Ipv4Addr::UNSPECIFIED).unwrap();
    assert_eq!(
        net.protocol().unwrap().lock().unwrap().route(local, None),
        Err(SocketError::NetworkUnreachable)
    );
    sender.close_description();
    receiver.close_description();
}
#[test]
fn actual_static_gateway_udp_and_asymmetric_protocol_reply_use_same_fib_and_wire() {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89235);
    let local_a = Ipv4Addr::new(10, 40, 0, 15);
    let local_b = Ipv4Addr::new(10, 41, 0, 15);
    let gateway = Ipv4Addr::new(10, 41, 0, 1);
    let peer = InetAddress {
        address: Ipv4Addr::new(203, 0, 113, 9),
        port: 44331,
    };
    let route = super::route::Ipv4RouteRequest {
        gateway: Some(gateway),
        scope: 0,
        ..static_route(Ipv4Addr::new(203, 0, 113, 0), 24, 3, 7)
    };
    configure_route(&net, route, false, false, true, false, false).unwrap();
    b.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local_b, gateway));
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    let sender = scoped_endpoint(&net, InetProtocol::Udp);
    sender
        .send_to(b"through-gateway", Some(peer), None)
        .unwrap();
    advance_actual_domains();
    assert!(a.wire.lock().unwrap().1.is_empty());
    assert!(b.wire.lock().unwrap().1.iter().any(|frame| {
        let eth = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        ip.dst_addr() == peer.address
            && ip.src_addr() == local_b
            && ip.next_header() == IpProtocol::Udp
            && UdpPacket::new_checked(ip.payload()).unwrap().payload() == b"through-gateway"
    }));
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    // 输入IP到A，回复peer的实际静态route经B/GW。没有用户socket触发这个ICMP codec。
    a.wire.lock().unwrap().0.push_back(peer_udp_frame_for(
        peer,
        InetAddress {
            address: local_a,
            port: 44332,
        },
        b"unbound-port",
    ));
    advance_actual_domains();
    assert!(a.wire.lock().unwrap().1.is_empty(), "IP回复不能继承输入NIC");
    assert!(b.wire.lock().unwrap().1.iter().any(|frame| {
        let eth = EthernetFrame::new_checked(frame.as_slice()).unwrap();
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        if ip.next_header() != IpProtocol::Icmp
            || ip.dst_addr() != peer.address
            || ip.src_addr() != local_a
        {
            return false;
        }
        matches!(
            Icmpv4Repr::parse(
                &Icmpv4Packet::new_checked(ip.payload()).unwrap(),
                &ChecksumCapabilities::default()
            )
            .unwrap(),
            Icmpv4Repr::DstUnreachable { .. }
        )
    }));
    assert!(
        !net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .route_replies
            .due(now() + smoltcp::time::Duration::from_millis(2000))
    );
    sender.close_description();
}
fn routing_tcp_syn(source: Ipv4Addr, destination: Ipv4Addr, port: u16) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let mut bytes = peer_tcp_frame(port, TcpControl::Syn, None);
    let mut ethernet = EthernetFrame::new_unchecked(bytes.as_mut_slice());
    let mut ip = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.set_src_addr(source);
    ip.set_dst_addr(destination);
    ip.fill_checksum();
    TcpPacket::new_unchecked(ip.payload_mut())
        .fill_checksum(&IpAddress::Ipv4(source), &IpAddress::Ipv4(destination));
    // 此fixture只生成peer wire，不实现任何路由或SYN握手policy。
    let ethernet = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
    let packet = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
    Ipv4Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap();
    bytes
}
#[test]
fn actual_tcp_passive_advertisement_uses_reverse_fib_mtu_and_actual_bound_reply_flow() {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89236);
    let local_a = Ipv4Addr::new(10, 40, 0, 15);
    let local_b = Ipv4Addr::new(10, 41, 0, 15);
    let gateway = Ipv4Addr::new(10, 41, 0, 1);
    let peer = Ipv4Addr::new(203, 0, 113, 19);
    let route = super::route::Ipv4RouteRequest {
        gateway: Some(gateway),
        scope: 0,
        ..static_route(Ipv4Addr::new(203, 0, 113, 0), 24, 3, 1)
    };
    configure_route(&net, route, false, false, true, false, false).unwrap();
    configure_mtu(&net, 3, 512).unwrap();
    for (device, local, neighbor) in [(&a, local_a, peer), (&b, local_b, gateway)] {
        device
            .wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, neighbor));
    }
    advance_actual_domains();
    a.wire.lock().unwrap().1.clear();
    b.wire.lock().unwrap().1.clear();
    let unbound = scoped_endpoint(&net, InetProtocol::Tcp);
    unbound
        .bind(InetAddress {
            address: local_a,
            port: 44334,
        })
        .unwrap();
    unbound.listen(1).unwrap();
    let bound = scoped_endpoint(&net, InetProtocol::Tcp);
    bound
        .bind_to_device(b"eth0", &crate::namespace::root_authority())
        .unwrap();
    bound
        .bind(InetAddress {
            address: local_a,
            port: 44335,
        })
        .unwrap();
    bound.listen(1).unwrap();
    for port in [44334, 44335] {
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(routing_tcp_syn(peer, local_a, port));
    }
    advance_actual_domains();
    let syn_mss = |device: &Arc<crate::drivers::WireDevice>, port| {
        device.wire.lock().unwrap().1.iter().find_map(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                return None;
            }
            let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp
                || ip.dst_addr() != peer
                || ip.src_addr() != local_a
            {
                return None;
            }
            let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
            if tcp.src_port() != port || !tcp.syn() || !tcp.ack() {
                return None;
            }
            TcpRepr::parse(
                &tcp,
                &IpAddress::Ipv4(local_a),
                &IpAddress::Ipv4(peer),
                &ChecksumCapabilities::default(),
            )
            .unwrap()
            .max_seg_size
        })
    };
    assert_eq!(syn_mss(&b, 44334), Some(472));
    assert_eq!(syn_mss(&a, 44334), None);
    assert_eq!(syn_mss(&a, 44335), Some(1460));
    assert_eq!(syn_mss(&b, 44335), None);
    unbound.close_description();
    bound.close_description();
    advance_actual_domains();
}
#[test]
fn actual_arp_weak_host_and_duplicate_address_probe_share_fib_but_keep_physical_link() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89237);
    let local_b = Ipv4Addr::new(10, 41, 0, 15);
    // target属于B而ARP物理输入A；native arp_ignore=0按本域LOCAL回答。
    for source in [Ipv4Addr::new(198, 51, 100, 17), Ipv4Addr::UNSPECIFIED] {
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local_b, source));
        advance_actual_domains();
        assert!(a.wire.lock().unwrap().1.iter().any(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            if ethernet.ethertype() != EthernetProtocol::Arp { return false; }
            matches!(ArpRepr::parse(&ArpPacket::new_checked(ethernet.payload()).unwrap()).unwrap(),
                ArpRepr::EthernetIpv4 { operation: ArpOperation::Reply, source_protocol_addr, target_protocol_addr, .. }
                if source_protocol_addr == local_b && target_protocol_addr == source)
        }));
        assert!(
            b.wire.lock().unwrap().1.is_empty(),
            "ARP不是routed IP reply，必须留在原物理link"
        );
        a.wire.lock().unwrap().1.clear();
    }
    assert_eq!(
        interface_snapshot(&net, 2).unwrap().address,
        Some(Ipv4Addr::new(10, 40, 0, 15))
    );
}

#[test]
fn actual_route_metrics_drive_advertisement_receive_window_and_independent_sender_mtu() {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, _) = routing_domain(89238);
    let local = Ipv4Addr::new(10, 40, 0, 15);
    let peer = Ipv4Addr::new(10, 40, 0, 16);
    configure_mtu(&net, 2, 256).unwrap();
    let route = super::route::Ipv4RouteRequest {
        metrics: super::route::Ipv4RouteMetrics {
            advertised_mss: 4096,
            receive_window: 777,
            round_trip_time: 8000,
        },
        ..static_route(peer, 32, 2, 1)
    };
    configure_route(&net, route, false, false, true, false, false).unwrap();
    let listener = scoped_endpoint(&net, InetProtocol::Tcp);
    listener
        .bind(InetAddress {
            address: local,
            port: 44336,
        })
        .unwrap();
    listener.listen(1).unwrap();
    a.wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local, peer));
    a.wire
        .lock()
        .unwrap()
        .0
        .push_back(routing_tcp_syn(peer, local, 44336));
    advance_actual_domains();
    let syn = a
        .wire
        .lock()
        .unwrap()
        .1
        .iter()
        .find_map(|bytes| {
            let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                return None;
            }
            let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp {
                return None;
            }
            let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
            if tcp.src_port() != 44336 || !tcp.syn() {
                return None;
            }
            let repr = TcpRepr::parse(
                &tcp,
                &IpAddress::Ipv4(local),
                &IpAddress::Ipv4(peer),
                &ChecksumCapabilities::default(),
            )
            .unwrap();
            assert_eq!(repr.max_seg_size, Some(4096));
            assert_eq!(repr.window_len, 777);
            Some(repr.seq_number + 1)
        })
        .unwrap();
    let mut ack = peer_tcp_frame(44336, TcpControl::None, Some(syn));
    let mut eth = EthernetFrame::new_unchecked(ack.as_mut_slice());
    let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
    ip.set_src_addr(peer);
    ip.set_dst_addr(local);
    ip.fill_checksum();
    TcpPacket::new_unchecked(ip.payload_mut())
        .fill_checksum(&IpAddress::Ipv4(peer), &IpAddress::Ipv4(local));
    a.wire.lock().unwrap().0.push_back(ack);
    advance_actual_domains();
    let child = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::socket::SocketMemory::try_new(net.clone()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        child
            .option(super::super::InternetOption::MaxSegment)
            .unwrap(),
        216,
        "4096 receive advertisement must not enlarge the actual sender PMTU cache"
    );
    child.set_no_delay(true).unwrap();
    a.wire.lock().unwrap().1.clear();
    child.send_to(&vec![0x71; 600], None, None).unwrap();
    let data_time = now();
    // 每轮同一 socket 最多发送一个 segment；先推进有界批次，避免把剩余数据的 PollAt::Now 当作 RTO。
    for _ in 0..4 { net.protocol().unwrap().lock().unwrap().poll_at(data_time); }
    let mut sent = 0;
    for bytes in &a.wire.lock().unwrap().1 {
        let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
        if eth.ethertype() != EthernetProtocol::Ipv4 {
            continue;
        }
        let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
        if ip.next_header() != IpProtocol::Tcp {
            continue;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        if tcp.src_port() != 44336 {
            continue;
        }
        assert!(ip.total_len() <= 256);
        assert!(tcp.payload().len() <= 216);
        assert!(tcp.window_len() <= 777);
        sent += tcp.payload().len();
    }
    assert!(sent > 0);
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        let NetworkStack {
            sockets,
            interfaces,
            ..
        } = &mut *network;
        let interface = interfaces
            .iter_mut()
            .find(|interface| interface.index == 2)
            .unwrap();
        assert_eq!(
            interface.interface.poll_at(data_time, sockets),
            Some(data_time + smoltcp::time::Duration::from_millis(3000)),
            "route IRTT seeds only the first data RTO after actual handshake"
        );
    }
    configure_mtu(&net, 2, 128).unwrap();
    net.protocol().unwrap().lock().unwrap().poll_at(data_time);
    assert_eq!(
        child
            .option(super::super::InternetOption::MaxSegment)
            .unwrap(),
        88,
        "actual route MTU refresh must update sender cache without copying the 4096 receive advertisement"
    );
    let snapshot = route_snapshot(&net).unwrap();
    assert!(snapshot.iter().any(|row| row.route == route));
    child.close_description();
    listener.close_description();
    advance_actual_domains();
}
#[test]
fn actual_tcp_timeout_keeps_progress_without_route_and_reports_once() {
    use smoltcp::time::Duration;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89239);
    let peer = Ipv4Addr::new(203, 0, 113, 31);
    let route = static_route(peer, 32, 2, 1);
    configure_route(&net, route, false, false, true, false, false).unwrap();
    let endpoint = scoped_endpoint(&net, InetProtocol::Tcp);
    assert_eq!(
        endpoint.connect(InetAddress {
            address: peer,
            port: 44337
        }),
        Err(SocketError::InProgress)
    );
    let InetEndpoint::Tcp(id) = endpoint.endpoint else {
        unreachable!()
    };
    let deadline = {
        let mut network = net.protocol().unwrap().lock().unwrap();
        let handle = network.tcp_endpoints[&id].first_handle_for_test();
        let socket = network
            .sockets
            .get_mut::<smoltcp::socket::tcp::Socket>(handle);
        socket.set_timeout(Some(Duration::from_millis(500)));
        socket.maintenance_deadline().unwrap()
    };
    configure_route(&net, route, true, false, false, false, false).unwrap();
    configure_up(&net, 2, false).unwrap();
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        assert_eq!(
            network.route(peer, None),
            Err(SocketError::NetworkUnreachable)
        );
        tcp::maintain_clocks(&mut network, deadline - Duration::from_millis(1));
        assert_eq!(network.tcp_endpoints[&id].pending_error_for_test(), None);
        tcp::maintain_clocks(&mut network, deadline);
        assert_eq!(
            network.tcp_endpoints[&id].pending_error_for_test(),
            Some(SocketError::TimedOut)
        );
    }
    assert_eq!(endpoint.take_error(), Some(SocketError::TimedOut));
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        tcp::maintain_clocks(&mut network, deadline + Duration::from_millis(1000));
    }
    assert_eq!(endpoint.take_error(), None);
    endpoint.close_description();
}

#[test]
fn actual_tcp_keepalive_probes_are_budgeted_counted_only_after_transmit_and_replies_reset_cycle() {
    use super::super::InternetOption as O;
    use smoltcp::{phy::ChecksumCapabilities, time::Duration, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for route_lost in [false, true] {
        let (net, a, _) = routing_domain(89240 + u32::from(route_lost));
        let local = Ipv4Addr::new(10, 40, 0, 15);
        let peer = Ipv4Addr::new(10, 40, 0, 16);
        let listener = scoped_endpoint(&net, InetProtocol::Tcp);
        for (option, value) in [
            (O::KeepAlive, 1),
            (O::KeepIdle, 1),
            (O::KeepInterval, 1),
            (O::KeepCount, 2),
        ] {
            listener.set_timer_option(option, value).unwrap();
            assert_eq!(listener.option(option).unwrap(), value);
        }
        for (option, value) in [
            (O::KeepIdle, 0),
            (O::KeepInterval, 32768),
            (O::KeepCount, 128),
        ] {
            assert_eq!(
                listener.set_timer_option(option, value),
                Err(SocketError::Invalid)
            );
        }
        listener
            .bind(InetAddress {
                address: local,
                port: 44338,
            })
            .unwrap();
        listener.listen(1).unwrap();
        let start = now();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, peer));
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(routing_tcp_syn(peer, local, 44338));
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let ack_sequence = a
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .find_map(|bytes| {
                let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
                if eth.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
                if ip.next_header() != IpProtocol::Tcp {
                    return None;
                }
                let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                (tcp.src_port() == 44338 && tcp.syn()).then(|| tcp.seq_number() + 1)
            })
            .unwrap();
        let response = || {
            let mut bytes = peer_tcp_frame(44338, TcpControl::None, Some(ack_sequence));
            let mut eth = EthernetFrame::new_unchecked(bytes.as_mut_slice());
            let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
            ip.set_src_addr(peer);
            ip.set_dst_addr(local);
            ip.fill_checksum();
            TcpPacket::new_unchecked(ip.payload_mut())
                .fill_checksum(&IpAddress::Ipv4(peer), &IpAddress::Ipv4(local));
            bytes
        };
        a.wire.lock().unwrap().0.push_back(response());
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let child = listener
            .accept(
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::socket::SocketMemory::try_new(net.clone()).unwrap(),
            )
            .unwrap();
        listener.set_timer_option(O::KeepCount, 9).unwrap();
        assert_eq!(
            child.option(O::KeepCount).unwrap(),
            2,
            "actual accepted child keeps final ACK policy"
        );
        if route_lost {
            configure_up(&net, 2, false).unwrap();
            a.wire.lock().unwrap().1.clear();
            let InetEndpoint::Tcp(id) = child.endpoint else {
                unreachable!()
            };
            for millis in [1000, 2000] {
                let timestamp = start + Duration::from_millis(millis);
                net.protocol().unwrap().lock().unwrap().poll_at(timestamp);
                assert_eq!(child.take_error(), None);
                let network = net.protocol().unwrap().lock().unwrap();
                let socket = network.sockets.get::<smoltcp::socket::tcp::Socket>(
                    network.tcp_endpoints[&id].first_handle_for_test(),
                );
                assert_eq!(
                    socket.maintenance_deadline(),
                    Some(timestamp + Duration::from_millis(1000)),
                    "actual route failure consumes one probe and schedules a future deadline"
                );
            }
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(start + Duration::from_millis(3000));
            assert_eq!(child.take_error(), Some(SocketError::TimedOut));
            assert_eq!(child.take_error(), None);
            assert!(
                a.wire.lock().unwrap().1.is_empty(),
                "failed route must not fake successful wire transmission"
            );
            child.close_description();
            listener.close_description();
            continue;
        }
        let is_probe = |bytes: &Vec<u8>| {
            let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                return false;
            }
            let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp {
                return false;
            }
            let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
            let repr = TcpRepr::parse(
                &tcp,
                &IpAddress::Ipv4(local),
                &IpAddress::Ipv4(peer),
                &ChecksumCapabilities::default(),
            )
            .unwrap();
            tcp.src_port() == 44338
                && repr.control == TcpControl::None
                && repr.seq_number == ack_sequence - 1
                && repr.ack_number == Some(TcpSeqNumber(1001))
                && repr.payload.is_empty()
        };
        a.wire.lock().unwrap().1.clear();
        a.transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        for millis in [1000, 2000, 3000] {
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(start + Duration::from_millis(millis));
        }
        assert_eq!(
            child.take_error(),
            None,
            "local TX congestion cannot consume peer probe count"
        );
        a.transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(start + Duration::from_millis(3500));
        assert_eq!(
            a.wire
                .lock()
                .unwrap()
                .1
                .iter()
                .filter(|bytes| is_probe(bytes))
                .count(),
            1
        );
        a.wire.lock().unwrap().0.push_back(response());
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(start + Duration::from_millis(4000));
        assert_eq!(child.take_error(), None);
        a.wire.lock().unwrap().1.clear();
        for millis in [5000, 6000] {
            net.protocol()
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(start + Duration::from_millis(millis));
        }
        assert_eq!(
            a.wire
                .lock()
                .unwrap()
                .1
                .iter()
                .filter(|bytes| is_probe(bytes))
                .count(),
            2
        );
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(start + Duration::from_millis(7000));
        assert_eq!(child.take_error(), Some(SocketError::TimedOut));
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(start + Duration::from_millis(8000));
        assert_eq!(child.take_error(), None);
        child.close_description();
        listener.close_description();
    }
}

#[test]
fn actual_tcp_syn_budget_and_user_timeout_expire_without_output_routes() {
    use super::super::InternetOption as O;
    use smoltcp::time::Duration;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89241);
    let peer = Ipv4Addr::new(203, 0, 113, 32);
    let route = static_route(peer, 32, 2, 1);
    configure_route(&net, route, false, false, true, false, false).unwrap();
    let retry = scoped_endpoint(&net, InetProtocol::Tcp);
    let timed = scoped_endpoint(&net, InetProtocol::Tcp);
    assert_eq!(retry.option(O::SynRetries).unwrap(), 6);
    assert_eq!(timed.option(O::UserTimeout).unwrap(), 0);
    retry.set_timer_option(O::SynRetries, 1).unwrap();
    timed.set_timer_option(O::UserTimeout, 100).unwrap();
    for (option, value) in [
        (O::SynRetries, 0),
        (O::SynRetries, 128),
        (O::UserTimeout, -1),
    ] {
        assert_eq!(
            retry.set_timer_option(option, value),
            Err(SocketError::Invalid)
        );
    }
    for endpoint in [&retry, &timed] {
        assert_eq!(
            endpoint.connect(InetAddress {
                address: peer,
                port: 44339
            }),
            Err(SocketError::InProgress)
        );
    }
    let deadline = |endpoint: &Arc<InetSocket>| {
        let InetEndpoint::Tcp(id) = endpoint.endpoint else {
            unreachable!()
        };
        let network = net.protocol().unwrap().lock().unwrap();
        network
            .sockets
            .get::<smoltcp::socket::tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test())
            .maintenance_deadline()
            .unwrap()
    };
    let retry_start = deadline(&retry) - Duration::from_millis(1000);
    let timed_start = deadline(&timed) - Duration::from_millis(1000);
    configure_route(&net, route, true, false, false, false, false).unwrap();
    configure_up(&net, 2, false).unwrap();
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        tcp::maintain_clocks(&mut network, timed_start + Duration::from_millis(1000));
    }
    assert_eq!(
        timed.take_error(),
        None,
        "native user timeout first gives SYN its initial 1s RTO"
    );
    assert_eq!(deadline(&timed), timed_start + Duration::from_millis(1002));
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        tcp::maintain_clocks(&mut network, timed_start + Duration::from_millis(1002));
    }
    assert_eq!(timed.take_error(), Some(SocketError::TimedOut));
    // native first four active SYN RTOs are linear; retry boundary is still a native elapsed model.
    for millis in [1500, 2000] {
        net.protocol()
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(retry_start + Duration::from_millis(millis));
        assert_eq!(retry.take_error(), None);
    }
    net.protocol()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(retry_start + Duration::from_millis(3000));
    assert_eq!(retry.take_error(), Some(SocketError::TimedOut));
    assert_eq!(retry.take_error(), None);
    retry.close_description();
    timed.close_description();
}

#[test]
fn actual_zero_window_clock_preserves_user_deadline_and_unrouted_probe_budget() {
    use super::super::InternetOption as O;
    use smoltcp::{time::Duration, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for scenario in 0..3u32 {
        let (net, a, _) = routing_domain(89245 + scenario);
        let local = Ipv4Addr::new(10, 40, 0, 15);
        let peer = Ipv4Addr::new(10, 40, 0, 16);
        let port = 44345 + scenario as u16;
        let listener = scoped_endpoint(&net, InetProtocol::Tcp);
        if scenario == 0 {
            listener.set_timer_option(O::UserTimeout, 1000).unwrap();
        }
        listener
            .bind(InetAddress {
                address: local,
                port,
            })
            .unwrap();
        listener.listen(1).unwrap();
        let start = now();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, peer));
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(routing_tcp_syn(peer, local, port));
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let ack_sequence = a
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .find_map(|bytes| {
                let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
                if eth.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
                if ip.next_header() != IpProtocol::Tcp {
                    return None;
                }
                let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                (tcp.src_port() == port && tcp.syn()).then_some(tcp.seq_number() + 1)
            })
            .unwrap();
        let response = || {
            let mut bytes = peer_tcp_frame(port, TcpControl::None, Some(ack_sequence));
            let mut eth = EthernetFrame::new_unchecked(bytes.as_mut_slice());
            let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
            ip.set_src_addr(peer);
            ip.set_dst_addr(local);
            ip.fill_checksum();
            let mut tcp = TcpPacket::new_unchecked(ip.payload_mut());
            tcp.set_window_len(0);
            tcp.fill_checksum(&IpAddress::Ipv4(peer), &IpAddress::Ipv4(local));
            bytes
        };
        a.wire.lock().unwrap().0.push_back(response());
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let child = listener
            .accept(
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::socket::SocketMemory::try_new(net.clone()).unwrap(),
            )
            .unwrap();
        child.send_to(b"x", None, None).unwrap();
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let InetEndpoint::Tcp(id) = child.endpoint else {
            unreachable!()
        };
        let deadline = || {
            let network = net.protocol().unwrap().lock().unwrap();
            network
                .sockets
                .get::<smoltcp::socket::tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test())
                .maintenance_deadline()
                .unwrap()
        };
        let first = deadline();
        assert!(
            first > start,
            "first zero-window probe is based on actual enqueue clock, not uptime zero"
        );
        if scenario == 1 {
            configure_up(&net, 2, false).unwrap();
            for _ in 0..15 {
                let next = deadline();
                net.protocol().unwrap().lock().unwrap().poll_at(next);
                assert_eq!(child.take_error(), None);
            }
            let next = deadline();
            net.protocol().unwrap().lock().unwrap().poll_at(next);
            assert_eq!(child.take_error(), Some(SocketError::TimedOut));
            assert_eq!(child.take_error(), None);
        } else {
            a.wire.lock().unwrap().1.clear();
            net.protocol().unwrap().lock().unwrap().poll_at(first);
            let probes = a
                .wire
                .lock()
                .unwrap()
                .1
                .iter()
                .filter(|bytes| {
                    let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
                    if eth.ethertype() != EthernetProtocol::Ipv4 {
                        return false;
                    }
                    let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
                    if ip.next_header() != IpProtocol::Tcp {
                        return false;
                    }
                    let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
                    tcp.src_port() == port
                        && tcp.ack()
                        && !tcp.fin()
                        && tcp.seq_number() == ack_sequence - 1
                        && tcp.payload().is_empty()
                })
                .count();
            assert_eq!(
                probes, 1,
                "native PROBE0 is one empty snd_una-1 ACK, no stream sequence consumption"
            );
            if scenario == 0 {
                for elapsed in [100, 350, 600, 850] {
                    a.wire.lock().unwrap().0.push_back(response());
                    net.protocol()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .poll_at(first + Duration::from_millis(elapsed));
                    assert_eq!(child.take_error(), None);
                }
                net.protocol()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .poll_at(first + Duration::from_millis(999));
                assert_eq!(child.take_error(), None);
                net.protocol()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .poll_at(first + Duration::from_millis(1000));
                assert_eq!(
                    child.take_error(),
                    Some(SocketError::TimedOut),
                    "non-progress window ACK cannot extend USER_TIMEOUT"
                );
                assert_eq!(child.take_error(), None);
            } else {
                // RFC1122默认允许持续应答的零窗口，超过15次也不能按总次数误杀。
                for _ in 0..20 {
                    let next = deadline();
                    a.wire.lock().unwrap().0.push_back(response());
                    net.protocol()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .poll_at(next - Duration::from_millis(1));
                    assert_eq!(child.take_error(), None);
                }
            }
        }
        child.disconnect().unwrap();
        child.close_description();
        listener.close_description();
    }
}

#[test]
fn actual_single_nic_tcp_rejects_ack_of_unsent_data_and_fin_but_accepts_ack_after_rto() {
    use smoltcp::{socket::tcp, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    for close_write in [false, true] {
        let net = crate::socket::NetNamespace::create(
            crate::namespace::initial_user(),
            89260 + u32::from(close_write),
        )
        .unwrap();
        let device = Arc::new(crate::drivers::WireDevice::new());
        net.install_protocol(prepare_network(&net, &[device.clone() as Arc<dyn crate::drivers::network::NetworkDevice>], true).unwrap());
        let local = Ipv4Addr::new(10, 40, 0, 15);
        let peer = Ipv4Addr::new(10, 40, 0, 16);
        configure_address(&net, 2, local).unwrap();
        configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, 2, true).unwrap();
        let port = 44360 + u16::from(close_write);
        let listener = scoped_endpoint(&net, InetProtocol::Tcp);
        listener
            .bind(InetAddress {
                address: local,
                port,
            })
            .unwrap();
        listener.listen(1).unwrap();
        let start = now();
        device
            .wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_arp_frame_for(local, peer));
        device
            .wire
            .lock()
            .unwrap()
            .0
            .push_back(routing_tcp_syn(peer, local, port));
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let base = device
            .wire
            .lock()
            .unwrap()
            .1
            .iter()
            .find_map(|bytes| {
                let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
                if eth.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
                if ip.next_header() != IpProtocol::Tcp {
                    return None;
                }
                let packet = TcpPacket::new_checked(ip.payload()).unwrap();
                (packet.src_port() == port && packet.syn()).then_some(packet.seq_number() + 1)
            })
            .unwrap();
        let ack = |sequence| {
            let mut bytes = peer_tcp_frame(port, TcpControl::None, Some(sequence));
            let mut eth = EthernetFrame::new_unchecked(bytes.as_mut_slice());
            let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
            ip.set_src_addr(peer);
            ip.set_dst_addr(local);
            ip.fill_checksum();
            TcpPacket::new_unchecked(ip.payload_mut())
                .fill_checksum(&IpAddress::Ipv4(peer), &IpAddress::Ipv4(local));
            bytes
        };
        device.wire.lock().unwrap().0.push_back(ack(base));
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let child = listener
            .accept(
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::socket::SocketMemory::try_new(net.clone()).unwrap(),
            )
            .unwrap();
        let InetEndpoint::Tcp(id) = child.endpoint else {
            unreachable!()
        };
        device
            .transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        assert_eq!(child.send_to(b"abc", None, None), Ok(3));
        if close_write {
            child.shutdown(1).unwrap();
        }
        let end = base + if close_write { 4 } else { 3 };
        device.wire.lock().unwrap().0.push_back(ack(end));
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        {
            let network = net.protocol().unwrap().lock().unwrap();
            let socket = network
                .sockets
                .get::<tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test());
            assert_eq!(
                socket.send_queue(),
                3,
                "ACK of untransmitted bytes/FIN must not consume data"
            );
            assert_eq!(
                socket.state(),
                if close_write {
                    tcp::State::FinWait1
                } else {
                    tcp::State::Established
                }
            );
        }
        device.wire.lock().unwrap().1.clear();
        device
            .transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        assert!(device.wire.lock().unwrap().1.iter().any(|bytes| {
            let eth = EthernetFrame::new_checked(bytes.as_slice()).unwrap();
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                return false;
            }
            let ip = Ipv4Packet::new_checked(eth.payload()).unwrap();
            if ip.next_header() != IpProtocol::Tcp {
                return false;
            }
            let packet = TcpPacket::new_checked(ip.payload()).unwrap();
            packet.src_port() == port
                && packet.seq_number() == base
                && packet.payload() == b"abc"
                && packet.fin() == close_write
        }));
        let rto = {
            let network = net.protocol().unwrap().lock().unwrap();
            network
                .sockets
                .get::<tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test())
                .maintenance_deadline()
                .unwrap()
        };
        device
            .transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        net.protocol().unwrap().lock().unwrap().poll_at(rto);
        device.wire.lock().unwrap().0.push_back(ack(end));
        net.protocol().unwrap().lock().unwrap().poll_at(rto);
        {
            let network = net.protocol().unwrap().lock().unwrap();
            let socket = network
                .sockets
                .get::<tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test());
            assert_eq!(
                socket.send_queue(),
                0,
                "legitimate old transmission ACK remains valid after cursor rewind"
            );
            assert_eq!(
                socket.state(),
                if close_write {
                    tcp::State::FinWait2
                } else {
                    tcp::State::Established
                }
            );
        }
        assert_eq!(child.take_error(), None);
        child.disconnect().unwrap();
        child.close_description();
        listener.close_description();
    }
}

#[test]
fn actual_single_nic_udp_and_raw_reject_oversize_before_queue_publication() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::socket::NetNamespace::create(crate::namespace::initial_user(), 89262).unwrap();
    let device = Arc::new(crate::drivers::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[device.clone() as Arc<dyn crate::drivers::network::NetworkDevice>], true).unwrap());
    let local = Ipv4Addr::new(10, 40, 0, 15);
    let peer = Ipv4Addr::new(10, 40, 0, 16);
    configure_address(&net, 2, local).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, 2, true).unwrap();
    configure_mtu(&net, 2, 128).unwrap();
    device
        .wire
        .lock()
        .unwrap()
        .0
        .push_back(peer_arp_frame_for(local, peer));
    net.protocol().unwrap().lock().unwrap().poll();
    device.wire.lock().unwrap().1.clear();
    for (kind, header, port) in [
        (InetProtocol::Udp, 28, 44362),
        (InetProtocol::Icmp, 20, 0),
    ] {
        let endpoint = scoped_endpoint(&net, kind);
        let target = InetAddress {
            address: peer,
            port,
        };
        let capacity = 128 - header;
        let input = vec![0u8; capacity + 1];
        assert_eq!(
            endpoint.send_to(&input, Some(target), None),
            Err(SocketError::MessageTooLarge)
        );
        net.protocol().unwrap().lock().unwrap().poll();
        assert!(device.wire.lock().unwrap().1.is_empty());
        assert_eq!(
            endpoint.send_to(&input[..capacity], Some(target), None),
            Ok(capacity)
        );
        net.protocol().unwrap().lock().unwrap().poll();
        let wire = device.wire.lock().unwrap();
        assert!(wire.1.iter().any(|frame| {
            let eth = EthernetFrame::new_checked(frame.as_slice()).unwrap();
            eth.ethertype() == EthernetProtocol::Ipv4
                && Ipv4Packet::new_checked(eth.payload()).unwrap().total_len() == 128
        }));
        drop(wire);
        device.wire.lock().unwrap().1.clear();
        endpoint.close_description();
    }
    let raw = scoped_endpoint(&net, InetProtocol::Icmp);
    let too_large = vec![0u8; 16 * 1024];
    assert_eq!(
        raw.send_to(
            &too_large,
            Some(InetAddress {
                address: Ipv4Addr::LOCALHOST,
                port: 0
            }),
            None
        ),
        Err(SocketError::MessageTooLarge),
        "a datagram that can never fit must not wait forever as EAGAIN"
    );
    raw.close_description();
}

#[test]
fn actual_single_nic_rejects_all_ipv4_fragments_without_reassembly() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::socket::NetNamespace::create(crate::namespace::initial_user(), 89263).unwrap();
    let device = Arc::new(crate::drivers::WireDevice::new());
    net.install_protocol(prepare_network(&net, &[device.clone() as Arc<dyn crate::drivers::network::NetworkDevice>], true).unwrap());
    let local = InetAddress { address: Ipv4Addr::new(10, 41, 0, 15), port: 44363 };
    let peer = InetAddress { address: Ipv4Addr::new(10, 41, 0, 16), port: 44000 };
    configure_address(&net, 2, local.address).unwrap();
    configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
    configure_up(&net, 2, true).unwrap();
    let receiver = scoped_endpoint(&net, InetProtocol::Udp);
    receiver.bind(local).unwrap();
    for (more, offset) in [(true, 0), (false, 8), (true, 8)] {
        let mut frame = peer_udp_frame_for(peer, local, b"fragment");
        let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
        let mut ip = Ipv4Packet::new_unchecked(ethernet.payload_mut());
        ip.set_dont_frag(false);
        ip.set_more_frags(more);
        ip.set_frag_offset(offset);
        // 零 UDP checksum 是合法 IPv4 输入，拒绝原因必须来自实际 fragment 机制。
        UdpPacket::new_unchecked(ip.payload_mut()).set_checksum(0);
        ip.fill_checksum();
        device.wire.lock().unwrap().0.push_back(frame);
        net.protocol().unwrap().lock().unwrap().poll();
        assert_eq!(udp_read(&receiver, false), Err(SocketError::Again));
    }
    device.wire.lock().unwrap().0.push_back(peer_udp_frame_for(peer, local, b"complete"));
    net.protocol().unwrap().lock().unwrap().poll();
    assert_eq!(udp_read(&receiver, false).unwrap().0, b"complete");
    receiver.close_description();
}
