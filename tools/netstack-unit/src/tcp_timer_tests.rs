#[test]
fn actual_tcp_timeout_keeps_progress_without_route_and_reports_once() {
    use smoltcp::time::Duration;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89239);
    let peer = Ipv4Addr::new(203, 0, 113, 31);
    let route = static_route(peer, 32, 2, 1);
    configure_route(&net, route, network_protocol::RouteChange { delete: false, replace: false, create: true, exclusive: false, append: false }).unwrap();
    let endpoint = scoped_endpoint(&net, InetProtocol::Tcp);
    assert_eq!(
        endpoint.connect(InetAddress::V4(core::net::SocketAddrV4::new(peer, 44337))),
        Err(SocketError::InProgress)
    );
    let InetEndpoint::Tcp(id) = endpoint.endpoint else {
        unreachable!()
    };
    let deadline = {
        let mut network = net.protocol().unwrap().lock().unwrap();
        let socket = network.tcp_endpoints.get_mut(&id).unwrap().transport_for_test_mut();
        socket.set_timeout(Some(::tcp::time::Duration::from_millis(500)));
        socket.maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros())).unwrap()
    };
    configure_route(&net, route, network_protocol::RouteChange { delete: true, replace: false, create: false, exclusive: false, append: false }).unwrap();
    configure_up(&net, 2, false).unwrap();
    {
        let mut network = net.protocol().unwrap().lock().unwrap();
        assert_eq!(
            network.route(Ipv4Addr::UNSPECIFIED, peer, None),
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
    use InternetOption as O;
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
            .bind(InetAddress::V4(core::net::SocketAddrV4::new(local, 44338)))
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
            .accept(Arc::new(crate::Event::default()))
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
                let socket = network.tcp_endpoints[&id].transport_for_test();
                assert_eq!(
                    socket.maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros())),
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
    use InternetOption as O;
    use smoltcp::time::Duration;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, _, _) = routing_domain(89241);
    let peer = Ipv4Addr::new(203, 0, 113, 32);
    let route = static_route(peer, 32, 2, 1);
    configure_route(&net, route, network_protocol::RouteChange { delete: false, replace: false, create: true, exclusive: false, append: false }).unwrap();
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
            endpoint.connect(InetAddress::V4(core::net::SocketAddrV4::new(peer, 44339))),
            Err(SocketError::InProgress)
        );
    }
    let deadline = |endpoint: &Arc<InetSocket>| {
        let InetEndpoint::Tcp(id) = endpoint.endpoint else {
            unreachable!()
        };
        let network = net.protocol().unwrap().lock().unwrap();
        network.tcp_endpoints[&id].transport_for_test()
            .maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros()))
            .unwrap()
    };
    let retry_start = deadline(&retry) - Duration::from_millis(1000);
    let timed_start = deadline(&timed) - Duration::from_millis(1000);
    configure_route(&net, route, network_protocol::RouteChange { delete: true, replace: false, create: false, exclusive: false, append: false }).unwrap();
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
    use InternetOption as O;
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
            .bind(InetAddress::V4(core::net::SocketAddrV4::new(local, port)))
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
            .accept(Arc::new(crate::Event::default()))
            .unwrap();
        child.send_to(b"x", None, &[]).unwrap();
        net.protocol().unwrap().lock().unwrap().poll_at(start);
        let InetEndpoint::Tcp(id) = child.endpoint else {
            unreachable!()
        };
        let deadline = || {
            let network = net.protocol().unwrap().lock().unwrap();
            network.tcp_endpoints[&id].transport_for_test()
                .maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros()))
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

