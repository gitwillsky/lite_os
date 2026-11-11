// 实际 TCP 连接、半关闭与最后描述符退休；accept policy 用例见 tcp_accept_tests.rs。

#[test]
fn receive_shutdown_keeps_queued_data_and_fin_eof_precedes_reset_error() {
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
    listener.bind(address(43009)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43009, TcpControl::Syn, None));
    let ack = syn_ack(43009);
    ingress(peer_tcp_frame(43009, TcpControl::None, Some(ack)));
    let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
    // native TCP 忽略有效目的地址，实际连接仍指向已建立的 peer。
    assert_eq!(child.send_to(&[], Some(address(9)), &[]).unwrap(), 0);
    ingress(peer_tcp_payload(
        43009,
        TcpControl::None,
        Some(ack),
        1001,
        b"abc",
    ));
    child.shutdown(0).unwrap();
    initial_stack().unwrap().lock().unwrap().poll();
    let mut data = [0; 3];
    assert_eq!(receive(&child, &mut data, true).unwrap().0, 3);
    assert_eq!(data, *b"abc");
    assert_eq!(receive(&child, &mut data, false).unwrap().0, 3);
    assert_eq!(receive(&child, &mut data, false).unwrap().0, 0);
    ingress(peer_tcp_payload(
        43009,
        TcpControl::Fin,
        Some(ack),
        1004,
        &[],
    ));
    ingress(peer_tcp_payload(
        43009,
        TcpControl::Rst,
        Some(ack),
        1005,
        &[],
    ));
    assert_eq!(receive(&child, &mut data, false).unwrap().0, 0);
    assert_eq!(
        child.take_error(),
        Some(SocketError::ConnectionReset),
        "FIN EOF preserves socket error for SO_ERROR"
    );
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(child.take_error(), None);
    child.close_description();
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn refused_active_connect_keeps_its_binding_and_does_not_refill_consumed_error() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    assert!(endpoint.poll_state().writable && endpoint.poll_state().hangup);
    let peer = InetAddress::V4(core::net::SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 2, 16),
        44000,
    ));
    ingress(peer_arp_frame());
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    initial_stack().unwrap().lock().unwrap().poll();
    let binding = endpoint.address().unwrap();
    let ack = {
        let mut wire = WIRE.lock().unwrap();
        wire.1
            .drain(..)
            .find_map(|bytes| {
                let ethernet = EthernetFrame::new_checked(&bytes[..]).ok()?;
                if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
                if ip.next_header() != IpProtocol::Tcp {
                    return None;
                }
                let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
                (tcp.src_port() == binding.port() && tcp.dst_port() == 44000 && tcp.syn())
                    .then(|| tcp.seq_number() + 1)
            })
            .expect("actual active protocol must emit SYN")
    };
    assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
    ingress(peer_tcp_frame(binding.port(), TcpControl::Rst, Some(ack)));
    assert_eq!(endpoint.address().unwrap(), binding);
    assert_eq!(
        endpoint.connection_result(),
        Err(SocketError::ConnectionRefused)
    );
    assert_eq!(endpoint.address().unwrap(), address(binding.port()));
    assert_eq!(endpoint.take_error(), None);
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(endpoint.take_error(), None);
    let mut byte = [0];
    assert!(matches!(
        receive(&endpoint, &mut byte, false),
        Err(SocketError::NotConnected)
    ));
    assert_eq!(
        endpoint.send_to(b"x", None, &[]),
        Err(SocketError::BrokenPipe)
    );
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn simultaneous_open_shutdown_waits_for_final_ack_before_connect_completion_and_fin() {
    use ::tcp::State; use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    endpoint.bind(address(43015)).unwrap();
    ingress(peer_arp_frame());
    assert_eq!(
        endpoint.connect(InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 2, 16),
            44000
        ))),
        Err(SocketError::InProgress)
    );
    initial_stack().unwrap().lock().unwrap().poll();
    ingress(peer_tcp_frame(43015, TcpControl::Syn, None));
    let ack = syn_ack(43015);
    endpoint.shutdown(1).unwrap();
    assert_eq!(endpoint.connection_result(), Err(SocketError::InProgress));
    assert_eq!(endpoint.send_to(b"x", None, &[]), Err(SocketError::Again));
    assert!(!endpoint.poll_state().writable);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        assert!(network.tcp_endpoints.values().any(|state| {
            let tcp=state.transport_for_test();
            tcp.local_endpoint()
                .is_some_and(|local| local.port == 43015)
                && tcp.state() == State::SynReceived
        }));
    }
    // RD shutdown 即时发布 EOF/RDHUP，但不得完成连接或让 SYN_RECV 发布 POLLOUT。
    endpoint.shutdown(0).unwrap();
    let readiness = endpoint.poll_state();
    assert!(readiness.readable && readiness.read_closed && readiness.hangup);
    assert!(!readiness.writable);
    let mut byte = [0];
    assert_eq!(receive(&endpoint, &mut byte, false).unwrap().0, 0);
    ingress(peer_tcp_frame(43015, TcpControl::None, Some(ack)));
    assert_eq!(endpoint.connection_result(), Ok(()));
    assert_eq!(
        endpoint.send_to(b"x", None, &[]),
        Err(SocketError::BrokenPipe)
    );
    assert!(endpoint.poll_state().writable);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        assert!(network.tcp_endpoints.values().any(|state| {
            let tcp=state.transport_for_test();
            tcp.local_endpoint()
                .is_some_and(|local| local.port == 43015)
                && tcp.state() == State::FinWait1
        }));
    }
    let fin_sent = WIRE.lock().unwrap().1.iter().any(|bytes| {
        let Ok(ethernet) = EthernetFrame::new_checked(&bytes[..]) else {
            return false;
        };
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return false;
        }
        let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
            return false;
        };
        if ip.next_header() != IpProtocol::Tcp {
            return false;
        }
        let Ok(tcp) = TcpPacket::new_checked(ip.payload()) else {
            return false;
        };
        tcp.src_port() == 43015 && tcp.dst_port() == 44000 && tcp.fin()
    });
    assert!(fin_sent, "actual final ACK must release the deferred FIN");
    ingress(peer_tcp_frame(43015, TcpControl::Rst, Some(ack)));
    endpoint.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn closed_shutdown_updates_readiness_and_bind_without_a_connected_transport() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    endpoint.readiness.signal(u32::MAX, 0).unwrap();
    assert_eq!(endpoint.shutdown(0), Err(SocketError::NotConnected));
    assert_ne!(
        endpoint
            .readiness
            .0
            .load(core::sync::atomic::Ordering::Relaxed)
            & native_abi::signals::USER_0,
        0
    );
    assert!(endpoint.poll_state().readable && endpoint.poll_state().read_closed);
    let mut byte = [0];
    assert_eq!(receive(&endpoint, &mut byte, false).unwrap().0, 0);
    // shutdown flags 不禁止尚未绑定的 TCP_CLOSE bind，AF_UNSPEC 则清除这些 flags。
    endpoint.bind(address(43011)).unwrap();
    endpoint.disconnect().unwrap();
    assert_eq!(endpoint.address().unwrap(), address(43011));
    assert_eq!(
        receive(&endpoint, &mut byte, false),
        Err(SocketError::NotConnected)
    );
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn syn_sent_shutdown_cancels_without_allocation_or_wire_reset_and_can_reconnect() {
    use smoltcp::wire::*;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    endpoint.bind(address(43012)).unwrap();
    ingress(peer_arp_frame());
    let peer = InetAddress::V4(core::net::SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 2, 16),
        44000,
    ));
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    initial_stack().unwrap().lock().unwrap().poll();
    WIRE.lock().unwrap().1.clear();
    endpoint.readiness.signal(u32::MAX, 0).unwrap();
    crate::allocation_failure::fail_next();
    let result = endpoint.shutdown(1);
    let allocated = crate::allocation_failure::was_consumed();
    assert!(!allocated, "SYN cancellation cannot depend on allocation");
    assert_eq!(result, Ok(()));
    assert_ne!(
        endpoint
            .readiness
            .0
            .load(core::sync::atomic::Ordering::Relaxed)
            & native_abi::signals::USER_0,
        0
    );
    assert_eq!(endpoint.address().unwrap(), address(43012));
    assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionReset));
    assert_eq!(counts(), (1, 0, 1));
    initial_stack().unwrap().lock().unwrap().poll();
    assert!(WIRE.lock().unwrap().1.iter().all(|bytes| {
        let Ok(ethernet) = EthernetFrame::new_checked(&bytes[..]) else {
            return true;
        };
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return true;
        }
        let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
            return true;
        };
        ip.next_header() != IpProtocol::Tcp || !TcpPacket::new_checked(ip.payload()).unwrap().rst()
    }));
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    assert_eq!(endpoint.take_error(), None);
    endpoint.disconnect().unwrap();
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn listener_read_shutdown_retires_children_and_relisten_uses_the_same_facade_slot() {
    use core::sync::atomic::Ordering::Relaxed;
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
    listener.bind(address(43013)).unwrap();
    listener.listen(3).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43013, TcpControl::Syn, None));
    let ack = syn_ack(43013);
    ingress(peer_tcp_frame(43013, TcpControl::None, Some(ack)));
    listener.bind_to_device(b"lo", true).unwrap();
    listener.shutdown(1).unwrap();
    assert!(
        listener.poll_state().readable,
        "listener SHUT_WR leaves accept intact"
    );
    TRANSMIT_AVAILABLE.store(false, Relaxed);
    let result = listener.shutdown(0);
    assert_eq!(result, Ok(()));
    assert_eq!(listener.address().unwrap(), address(43013));
    let released = scoped_endpoint(namespace(), InetProtocol::Tcp);
    released.bind_to_device(b"eth0", false).unwrap();
    released.bind(address(43013)).unwrap();
    released.close_description();
    assert!(!listener.poll_state().readable && listener.poll_state().hangup);
    assert!(matches!(
        listener.accept(Arc::new(crate::Event::default())),
        Err(SocketError::Invalid)
    ));
    // 重listen失败保留原bind/slot；RST只在唯一报文队列，不再占用orphan身份。
    crate::allocation_failure::fail_next();
    let result = listener.listen(3);
    assert!(crate::allocation_failure::was_consumed());
    assert_eq!(result, Err(SocketError::NoMemory));
    assert!(listener.poll_state().hangup);
    listener.listen(3).unwrap();
    assert_eq!(counts(), (3, 0, 1));
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(
        counts(),
        (3, 0, 1),
        "NIC backpressure cannot retain a closed TCP identity"
    );
    TRANSMIT_AVAILABLE.store(true, Relaxed);
    initial_stack()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(now() + smoltcp::time::Duration::from_millis(1000));
    assert_eq!(counts(), (3, 0, 1));
    listener.disconnect().unwrap();
    listener.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn abortive_last_fput_releases_the_port_before_reset_delivery() {
    use core::sync::atomic::Ordering::Relaxed;
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
    listener.bind(address(43014)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43014, TcpControl::Syn, None));
    let ack = syn_ack(43014);
    ingress(peer_tcp_frame(43014, TcpControl::None, Some(ack)));
    TRANSMIT_AVAILABLE.store(false, Relaxed);
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    replacement.bind(address(43014)).unwrap();
    TRANSMIT_AVAILABLE.store(true, Relaxed);
    // 首次失败发送只安排有界queue的本地重试；不持有旧TCP/端口。
    initial_stack()
        .unwrap()
        .lock()
        .unwrap()
        .poll_at(now() + smoltcp::time::Duration::from_millis(1000));
    {
        use smoltcp::wire::*;
        let wire = WIRE.lock().unwrap();
        let resets: Vec<_> = wire
            .1
            .iter()
            .filter_map(|frame| {
                let ethernet = EthernetFrame::new_checked(frame.as_slice()).ok()?;
                if ethernet.ethertype() != EthernetProtocol::Ipv4 {
                    return None;
                }
                let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
                if ip.next_header() != IpProtocol::Tcp {
                    return None;
                }
                let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
                (tcp.rst() && tcp.src_port() == 43014).then_some((
                    tcp.ack(),
                    tcp.window_len(),
                    tcp.payload().len(),
                    ip.verify_checksum(),
                    tcp.verify_checksum(
                        &IpAddress::Ipv4(ip.src_addr()),
                        &IpAddress::Ipv4(ip.dst_addr()),
                    ),
                ))
            })
            .collect();
        assert_eq!(
            resets.len(),
            1,
            "one actual RST, no transport retransmission"
        );
        assert_eq!(resets[0], (true, 0, 0, true, true));
    }
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn udp_actual_fput_releases_port_and_closed_observer_cannot_access_replacement() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    assert_eq!(counts(), (0, 0, 0));
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    endpoint.bind(address(43001)).unwrap();
    let observer = endpoint.clone();
    let handle = endpoint.udp_handle().unwrap();
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    assert_ne!(
        replacement.udp_handle().unwrap(),
        handle,
        "native endpoint identities never repeat"
    );
    replacement.bind(address(43001)).unwrap();
    endpoint.close_description();
    drop(endpoint);
    assert_eq!(
        observer.bind(address(43002)),
        Err(SocketError::NotConnected)
    );
    assert_eq!(observer.address(), Err(SocketError::NotConnected));
    assert_eq!(
        observer.send_to(b"stale", Some(address(43003)), &[]),
        Err(SocketError::NotConnected)
    );
    let mut bytes = [0; 16];
    assert_eq!(
        receive(&observer, &mut bytes, false),
        Err(SocketError::NotConnected)
    );
    assert_eq!(
        observer.set_reuse_address(true),
        Err(SocketError::NotConnected)
    );
    assert_eq!(observer.connection_result(), Err(SocketError::NotConnected));
    assert_eq!(observer.poll_state(), SocketPollState::closed());
    assert_eq!(observer.poll_state(), SocketPollState::closed());
    observer.close_description();
    drop(observer);
    assert_eq!(replacement.address().unwrap().port(), 43001);
    assert_eq!(counts(), (1, 1, 0));
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn closed_listener_observer_does_not_hold_port_or_retire_replacement() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    listener.bind(address(43004)).unwrap();
    listener.listen(3).unwrap();
    assert_eq!(counts(), (3, 0, 1));
    let observer = listener.clone();
    listener.close_description();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    replacement.bind(address(43004)).unwrap();
    replacement.listen(1).unwrap();
    assert_eq!(observer.listen(4), Err(SocketError::NotConnected));
    assert_eq!(
        observer.bind(address(43005)),
        Err(SocketError::NotConnected)
    );
    assert!(matches!(
        observer.accept(Arc::new(crate::Event::default())),
        Err(SocketError::NotConnected)
    ));
    observer.close_description();
    drop((listener, observer));
    assert_eq!(replacement.address().unwrap().port(), 43004);
    assert_eq!(counts(), (1, 0, 1));
    replacement.close_description();
}
#[test]
fn deferred_actual_close_releases_port_before_the_next_task_transaction() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    endpoint.bind(address(43006)).unwrap();
    let owner = initial_stack().unwrap();
    let guard = owner.lock().unwrap();
    crate::allocation_failure::fail_next();
    endpoint.close_description();
    assert!(
        !crate::allocation_failure::was_consumed(),
        "final cleanup publication must not allocate"
    );
    assert!(endpoint.operation.lock().is_none());
    assert_eq!(
        guard.udp_endpoints.len(),
        1,
        "busy owner keeps command-owned slot until drain"
    );
    drop(guard);
    let replacement = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    replacement.bind(address(43006)).unwrap();
    endpoint.close_description();
    assert_eq!(counts(), (1, 1, 0));
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn fresh_tcp_admissions_and_final_cleanup_share_the_actual_socket_capacity() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let mut endpoints = Vec::with_capacity(SOCKET_STORAGE_CAPACITY);
    for _ in 0..SOCKET_STORAGE_CAPACITY {
        endpoints.push(
            InetSocket::new(
                network_protocol::InetFamily::Ipv4,
                InetProtocol::Tcp,
                Arc::new(crate::Event::default()),
                namespace().clone(),
            )
            .unwrap(),
        );
    }
    assert_eq!(
        counts(),
        (SOCKET_STORAGE_CAPACITY, 0, SOCKET_STORAGE_CAPACITY)
    );
    // 既有容量用例只数 slot，不能发现每个 CLOSED socket 提前占用 64 KiB 的回归。
    // 检查实际协议缓冲总容量，不新增另一套分配计数或空闲状态模型。
    let payload_capacity: usize=initial_stack().unwrap().lock().unwrap().tcp_endpoints.values()
        .map(|state| {let socket=state.transport_for_test();socket.recv_capacity()+socket.send_capacity()}).sum();
    assert_eq!(payload_capacity, 0, "closed endpoints have no stream payload");
    assert!(matches!(
        InetSocket::new(
            network_protocol::InetFamily::Ipv4,
            InetProtocol::Tcp,
            Arc::new(crate::Event::default()),
            namespace().clone(),
        ),
        Err(SocketError::NoMemory)
    ));
    let owner = initial_stack().unwrap();
    let guard = owner.lock().unwrap();
    crate::allocation_failure::fail_next();
    for endpoint in &endpoints {
        endpoint.close_description();
    }
    assert!(
        !crate::allocation_failure::was_consumed(),
        "full deferred ring must not allocate or overflow"
    );
    drop(guard);
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    for endpoint in endpoints {
        endpoint.close_description();
        drop(endpoint);
    }
    assert_eq!(counts(), (1, 0, 1));
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn tcp_recreated_actual_transport_keeps_live_option_policy() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        namespace().clone(),
    )
    .unwrap();
    assert_eq!(endpoint.option(InternetOption::HopLimit).unwrap(), 64);
    endpoint.set_hop_limit(Some(29)).unwrap();
    endpoint.set_no_delay(true).unwrap();
    endpoint.set_max_segment(88).unwrap();
    let peer = InetAddress::V4(core::net::SocketAddrV4::new(
        Ipv4Addr::new(10, 0, 2, 16),
        44000,
    ));
    // 首次打开的两份流缓冲必须一起准备；任何一份 OOM 都保留原 CLOSED 绑定、policy 与句柄。
    for successful in 0..2 {
        crate::allocation_failure::fail_after(successful);
        assert_eq!(endpoint.connect(peer), Err(SocketError::NoMemory));
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(endpoint.address(), Ok(address(0)));
        assert_eq!(endpoint.option(InternetOption::MaxSegment), Ok(88));
    }
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    assert_eq!(endpoint.option(InternetOption::MaxSegment).unwrap(), 88);
    endpoint.disconnect().unwrap();
    assert_eq!(endpoint.option(InternetOption::MaxSegment).unwrap(), 88);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        let InetEndpoint::Tcp(id) = endpoint.endpoint else {
            unreachable!()
        };
        assert_eq!(network.tcp_endpoints[&id].options.user_mss, None);
        assert_eq!(
            network.tcp_endpoints[&id].transport_for_test()
                .send_mss(),
            88
        );
    }
    assert_eq!(endpoint.option(InternetOption::HopLimit).unwrap(), 29);
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    assert_eq!(endpoint.option(InternetOption::MaxSegment).unwrap(), 536);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        let InetEndpoint::Tcp(id) = endpoint.endpoint else {
            unreachable!()
        };
        assert!(tcp::transport_options_match(&network, id, true, Some(29)));
    }
    endpoint.set_hop_limit(None).unwrap();
    assert_eq!(endpoint.option(InternetOption::HopLimit).unwrap(), 64);
    endpoint.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn reset_allocation_failure_and_missing_route_do_not_retain_closed_tcp_or_port() {
    use smoltcp::wire::TcpControl;
    let _serial = SERIAL.lock().unwrap();
    for missing_route in [false, true] {
        initialize();
        let port = if missing_route { 43035 } else { 43034 };
        let listener = scoped_endpoint(&namespace().clone(), InetProtocol::Tcp);
        listener.bind(address(port)).unwrap();
        listener.listen(1).unwrap();
        ingress(peer_arp_frame());
        ingress(peer_tcp_frame(port, TcpControl::Syn, None));
        let ack = syn_ack(port);
        ingress(peer_tcp_frame(port, TcpControl::None, Some(ack)));
        if missing_route {
            configure_up(&namespace().clone(), 2, false).unwrap();
        } else {
            crate::allocation_failure::fail_next();
        }
        listener.close_description();
        if !missing_route {
            assert!(
                crate::allocation_failure::was_consumed(),
                "actual reset packet preparation must fail, not a synthetic error"
            );
        }
        assert_eq!(
            counts(),
            (0, 0, 0),
            "best-effort reset cannot retain transport after no route or OOM"
        );
        let replacement = scoped_endpoint(&namespace().clone(), InetProtocol::Tcp);
        replacement.bind(address(port)).unwrap();
        replacement.close_description();
        configure_up(&namespace().clone(), 2, true).unwrap();
    }
}

#[test]
fn actual_fin_wait2_linger_is_orphan_only_and_expires_without_route_or_interface() {
    use ::tcp::State; use smoltcp::{time::Duration, wire::*};
    let _serial = SERIAL.lock().unwrap();
    for (ordinal, seconds) in [-1, 1, 0, 120].into_iter().enumerate() {
        initialize();
        let port = 43040 + ordinal as u16;
        let listener = scoped_endpoint(&namespace().clone(), InetProtocol::Tcp);
        listener.bind(address(port)).unwrap();
        listener
            .set_timer_option(InternetOption::Linger2, seconds)
            .unwrap();
        listener.listen(1).unwrap();
        ingress(peer_arp_frame());
        ingress(peer_tcp_frame(port, TcpControl::Syn, None));
        let ack = syn_ack(port);
        ingress(peer_tcp_frame(port, TcpControl::None, Some(ack)));
        let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
        listener.close_description();
        let InetEndpoint::Tcp(id) = child.endpoint else {
            unreachable!()
        };
        child.shutdown(1).unwrap();
        initial_stack().unwrap().lock().unwrap().poll();
        let fin_ack = {
            let wire = WIRE.lock().unwrap();
            wire.1
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
                    (tcp.src_port() == port && tcp.fin()).then_some(tcp.seq_number() + 1)
                })
                .expect("actual protocol must publish FIN")
        };
        ingress(peer_tcp_frame(port, TcpControl::None, Some(fin_ack)));
        {
            let network = initial_stack().unwrap().lock().unwrap();
            let tcp = network.tcp_endpoints[&id].transport_for_test();
            assert_eq!(tcp.state(), State::FinWait2);
            assert_eq!(
                tcp.maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros())),
                None,
                "live SHUT_WR cannot acquire orphan FIN deadline"
            );
        }
        child.close_description();
        if seconds < 0 {
            assert_eq!(
                counts(),
                (0, 0, 0),
                "negative LINGER2 retires actual connection at final fput"
            );
            initial_stack().unwrap().lock().unwrap().poll();
        } else {
            let deadline = {
                let network = initial_stack().unwrap().lock().unwrap();
                network.tcp_endpoints[&id].transport_for_test()
                    .maintenance_deadline().map(|deadline| smoltcp::time::Instant::from_micros(deadline.total_micros()))
                    .unwrap()
            };
            let before = now();
            let expected_seconds = if seconds == 0 { 60 } else { seconds as u64 };
            assert!(deadline > before + Duration::from_millis(expected_seconds * 1000 - 100));
            configure_up(&namespace().clone(), 2, false).unwrap();
            initial_stack()
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(deadline - Duration::from_millis(1));
            assert_eq!(counts(), (1, 0, 1));
            initial_stack().unwrap().lock().unwrap().poll_at(deadline);
            assert_eq!(
                counts(),
                (0, 0, 0),
                "FIN_WAIT2 clock cannot depend on route or NIC"
            );
            configure_up(&namespace().clone(), 2, true).unwrap();
        }
        let replacement = scoped_endpoint(&namespace().clone(), InetProtocol::Tcp);
        replacement.bind(address(port)).unwrap();
        replacement.close_description();
    }
}
