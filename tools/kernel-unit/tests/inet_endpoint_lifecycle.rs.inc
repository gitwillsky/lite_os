// 实际 InetSocket、smoltcp SocketSet、port leases 与 deferred cleanup；只有 NIC/时钟/唤醒为边界。
use crate::net::NETWORK_TEST_SERIAL as SERIAL;
#[test]
fn passive_child_freezes_final_ack_policy_and_failed_accept_keeps_the_same_child() {
    use super::super::{SocketIoDirection::Receive, SocketTimeout};
    use smoltcp::wire::TcpControl;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43007)).unwrap();
    listener.set_hop_limit(Some(33)).unwrap();
    listener.set_max_segment(512).unwrap();
    listener.listen(2).unwrap();
    listener
        .set_timeout(Receive, SocketTimeout::finite(20_000_000))
        .unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43007, TcpControl::Syn, None));
    {
        use smoltcp::wire::*;
        let wire = crate::devices::model::WIRE.lock().unwrap();
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
    listener
        .set_timeout(Receive, SocketTimeout::finite(40_000_000))
        .unwrap();
    listener.set_no_delay(true).unwrap();
    listener.set_hop_limit(Some(41)).unwrap();
    listener.set_max_segment(256).unwrap();
    listener.set_reuse_address(true).unwrap();
    ingress(peer_tcp_frame(43007, TcpControl::None, Some(ack)));
    assert!(listener.poll_state().readable);
    listener
        .set_timeout(Receive, SocketTimeout::finite(60_000_000))
        .unwrap();
    listener.set_no_delay(false).unwrap();
    listener.set_hop_limit(Some(55)).unwrap();
    listener.set_max_segment(1024).unwrap();
    listener.set_reuse_address(false).unwrap();
    let notify = Pipe::notification_pair().unwrap();
    let policy = Arc::new(SocketTimeouts::new());
    let memory = crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap();
    crate::allocation_failure::fail_next();
    assert!(matches!(
        listener.accept(notify, policy, memory),
        Err(SocketError::NoMemory)
    ));
    assert!(crate::allocation_failure::was_consumed());
    assert!(
        listener.poll_state().readable,
        "OOM must retain actual child and frozen policy"
    );
    let child = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        child.timeouts.get(Receive),
        SocketTimeout::finite(40_000_000)
    );
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
    use super::super::InternetOption;
    assert_eq!(child.option(InternetOption::HopLimit).unwrap(), 41);
    assert_eq!(listener.option(InternetOption::HopLimit).unwrap(), 55);
    assert_eq!(child.option(InternetOption::NoDelay).unwrap(), 1);
    assert_eq!(listener.option(InternetOption::NoDelay).unwrap(), 0);
    assert_eq!(child.option(InternetOption::MaxSegment).unwrap(), 512);
    assert_eq!(listener.option(InternetOption::MaxSegment).unwrap(), 1024);
    // SYN 时的真实广告已固定；ESTABLISHED setter 只改变下一次 open policy，不伪造当前 MSS。
    child.set_max_segment(88).unwrap();
    assert_eq!(child.option(InternetOption::MaxSegment).unwrap(), 512);
    crate::devices::model::WIRE.lock().unwrap().1.clear();
    assert_eq!(child.send_to(&vec![0x35; 2000], None, None).unwrap(), 2000);
    initial_stack().unwrap().lock().unwrap().poll();
    {
        use smoltcp::wire::*;
        let wire = crate::devices::model::WIRE.lock().unwrap();
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
    child.set_timeout(Receive, SocketTimeout::Infinite).unwrap();
    assert_eq!(
        listener.timeouts.get(Receive),
        SocketTimeout::finite(60_000_000)
    );
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43008)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43008, TcpControl::Syn, None));
    let ack = syn_ack(43008);
    {
        let mut wire = crate::devices::model::WIRE.lock().unwrap();
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
    let child = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        child.address().unwrap(),
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 15),
            port: 43008
        }
    );
    assert_eq!(child.peer_address(), Err(SocketError::NotConnected));
    let mut empty = [];
    assert_eq!(
        child
            .receive(
                &mut crate::ipc::ReceiveBuffer::from_slice(&mut empty),
                false
            )
            .unwrap()
            .0,
        0
    );
    let mut byte = [0];
    assert!(matches!(
        child.receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut byte), false),
        Err(SocketError::ConnectionReset)
    ));
    assert_eq!(child.take_error(), None);
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(
        child.take_error(),
        None,
        "subsequent maintain cannot recreate RESET"
    );
    assert_eq!(
        child
            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut byte), false)
            .unwrap()
            .0,
        0
    );
    assert_eq!(
        child.send_to(b"x", None, None),
        Err(SocketError::BrokenPipe)
    );
    assert!(child.poll_state().hangup && child.poll_state().writable);
    assert!(!child.poll_state().error);
    child.close_description();
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn receive_shutdown_keeps_queued_data_and_fin_eof_precedes_reset_error() {
    use smoltcp::wire::TcpControl;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43009)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43009, TcpControl::Syn, None));
    let ack = syn_ack(43009);
    ingress(peer_tcp_frame(43009, TcpControl::None, Some(ack)));
    let child = listener
        .accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
        )
        .unwrap();
    // native TCP 忽略有效目的地址，实际连接仍指向已建立的 peer。
    assert_eq!(child.send_to(&[], Some(address(9)), None).unwrap(), 0);
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
    assert_eq!(
        child
            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut data), true)
            .unwrap()
            .0,
        3
    );
    assert_eq!(data, *b"abc");
    assert_eq!(
        child
            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut data), false)
            .unwrap()
            .0,
        3
    );
    assert_eq!(
        child
            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut data), false)
            .unwrap()
            .0,
        0
    );
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
    assert_eq!(
        child
            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut data), false)
            .unwrap()
            .0,
        0
    );
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    assert!(endpoint.poll_state().writable && endpoint.poll_state().hangup);
    let peer = InetAddress {
        address: Ipv4Addr::new(10, 0, 2, 16),
        port: 44000,
    };
    ingress(peer_arp_frame());
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    initial_stack().unwrap().lock().unwrap().poll();
    let binding = endpoint.address().unwrap();
    let ack = {
        let mut wire = crate::devices::model::WIRE.lock().unwrap();
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
                (tcp.src_port() == binding.port && tcp.dst_port() == 44000 && tcp.syn())
                    .then(|| tcp.seq_number() + 1)
            })
            .expect("actual active protocol must emit SYN")
    };
    assert_eq!(endpoint.peer_address(), Err(SocketError::NotConnected));
    ingress(peer_tcp_frame(binding.port, TcpControl::Rst, Some(ack)));
    assert_eq!(endpoint.address().unwrap(), binding);
    assert_eq!(
        endpoint.connection_result(),
        Err(SocketError::ConnectionRefused)
    );
    assert_eq!(endpoint.address().unwrap(), address(binding.port));
    assert_eq!(endpoint.take_error(), None);
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(endpoint.take_error(), None);
    let mut byte = [0];
    assert!(matches!(
        endpoint.receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut byte), false),
        Err(SocketError::NotConnected)
    ));
    assert_eq!(
        endpoint.send_to(b"x", None, None),
        Err(SocketError::BrokenPipe)
    );
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

fn counts() -> (usize, usize, usize) {
    let network = initial_stack().unwrap().lock().unwrap();
    (
        network.sockets.iter().count(),
        network.endpoints.len(),
        network.tcp_endpoints.len(),
    )
}

#[test]
fn simultaneous_open_shutdown_waits_for_final_ack_before_connect_completion_and_fin() {
    use smoltcp::{socket::tcp::State, wire::*};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    endpoint.bind(address(43015)).unwrap();
    ingress(peer_arp_frame());
    assert_eq!(
        endpoint.connect(InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 16),
            port: 44000,
        }),
        Err(SocketError::InProgress)
    );
    initial_stack().unwrap().lock().unwrap().poll();
    ingress(peer_tcp_frame(43015, TcpControl::Syn, None));
    let ack = syn_ack(43015);
    endpoint.shutdown(1).unwrap();
    assert_eq!(endpoint.connection_result(), Err(SocketError::InProgress));
    assert_eq!(endpoint.send_to(b"x", None, None), Err(SocketError::Again));
    assert!(!endpoint.poll_state().writable);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        assert!(network.sockets.iter().any(|(_, socket)| {
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                return false;
            };
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
    assert_eq!(
        endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut byte), false)
            .unwrap()
            .0,
        0
    );
    ingress(peer_tcp_frame(43015, TcpControl::None, Some(ack)));
    assert_eq!(endpoint.connection_result(), Ok(()));
    assert_eq!(
        endpoint.send_to(b"x", None, None),
        Err(SocketError::BrokenPipe)
    );
    assert!(endpoint.poll_state().writable);
    {
        let network = initial_stack().unwrap().lock().unwrap();
        assert!(network.sockets.iter().any(|(_, socket)| {
            let smoltcp::socket::Socket::Tcp(tcp) = socket else {
                return false;
            };
            tcp.local_endpoint()
                .is_some_and(|local| local.port == 43015)
                && tcp.state() == State::FinWait1
        }));
    }
    let fin_sent = crate::devices::model::WIRE.lock().unwrap().1.iter().any(|bytes| {
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    let generation = endpoint.readiness_generation();
    assert_eq!(endpoint.shutdown(0), Err(SocketError::NotConnected));
    assert_ne!(endpoint.readiness_generation(), generation);
    assert!(endpoint.poll_state().readable && endpoint.poll_state().read_closed);
    let mut byte = [0];
    assert_eq!(
        endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut byte), false)
            .unwrap()
            .0,
        0
    );
    // shutdown flags 不禁止尚未绑定的 TCP_CLOSE bind，AF_UNSPEC 则清除这些 flags。
    endpoint.bind(address(43011)).unwrap();
    endpoint.disconnect().unwrap();
    assert_eq!(endpoint.address().unwrap(), address(43011));
    assert_eq!(
        endpoint.receive(&mut ReceiveBuffer::from_slice(&mut byte), false),
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    endpoint.bind(address(43012)).unwrap();
    ingress(peer_arp_frame());
    let peer = InetAddress {
        address: Ipv4Addr::new(10, 0, 2, 16),
        port: 44000,
    };
    assert_eq!(endpoint.connect(peer), Err(SocketError::InProgress));
    initial_stack().unwrap().lock().unwrap().poll();
    crate::devices::model::WIRE.lock().unwrap().1.clear();
    let generation = endpoint.readiness_generation();
    crate::allocation_failure::fail_next();
    let result = endpoint.shutdown(1);
    let allocated = crate::allocation_failure::was_consumed();
    assert!(!allocated, "SYN cancellation cannot depend on allocation");
    assert_eq!(result, Ok(()));
    assert_ne!(endpoint.readiness_generation(), generation);
    assert_eq!(endpoint.address().unwrap(), address(43012));
    assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionReset));
    assert_eq!(counts(), (1, 0, 1));
    initial_stack().unwrap().lock().unwrap().poll();
    assert!(crate::devices::model::WIRE.lock().unwrap().1.iter().all(|bytes| {
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43013)).unwrap();
    listener.listen(3).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43013, TcpControl::Syn, None));
    let ack = syn_ack(43013);
    ingress(peer_tcp_frame(43013, TcpControl::None, Some(ack)));
    listener.shutdown(1).unwrap();
    assert!(
        listener.poll_state().readable,
        "listener SHUT_WR leaves accept intact"
    );
    crate::devices::model::TRANSMIT_AVAILABLE.store(false, Relaxed);
    let result = listener.shutdown(0);
    assert_eq!(result, Ok(()));
    assert_eq!(listener.address().unwrap(), address(43013));
    assert!(!listener.poll_state().readable && listener.poll_state().hangup);
    assert!(matches!(
        listener.accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap()
        ),
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
    crate::devices::model::TRANSMIT_AVAILABLE.store(true, Relaxed);
    initial_stack().unwrap().lock().unwrap().poll_at(now() + smoltcp::time::Duration::from_millis(1000));
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43014)).unwrap();
    listener.listen(1).unwrap();
    ingress(peer_arp_frame());
    ingress(peer_tcp_frame(43014, TcpControl::Syn, None));
    let ack = syn_ack(43014);
    ingress(peer_tcp_frame(43014, TcpControl::None, Some(ack)));
    crate::devices::model::TRANSMIT_AVAILABLE.store(false, Relaxed);
    listener.close_description();
    initial_stack().unwrap().lock().unwrap().poll();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    replacement.bind(address(43014)).unwrap();
    crate::devices::model::TRANSMIT_AVAILABLE.store(true, Relaxed);
    // 首次失败发送只安排有界queue的本地重试；不持有旧TCP/端口。
    initial_stack().unwrap().lock().unwrap().poll_at(now() + smoltcp::time::Duration::from_millis(1000));
    {
        use smoltcp::wire::*;
        let wire = crate::devices::model::WIRE.lock().unwrap();
        let resets: Vec<_> = wire.1.iter().filter_map(|frame| {
            let ethernet = EthernetFrame::new_checked(frame.as_slice()).ok()?;
            if ethernet.ethertype() != EthernetProtocol::Ipv4 { return None; }
            let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
            if ip.next_header() != IpProtocol::Tcp { return None; }
            let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
            (tcp.rst() && tcp.src_port() == 43014).then_some((
                tcp.ack(), tcp.window_len(), tcp.payload().len(),
                ip.verify_checksum(),
                tcp.verify_checksum(&IpAddress::Ipv4(ip.src_addr()), &IpAddress::Ipv4(ip.dst_addr())),
            ))
        }).collect();
        assert_eq!(resets.len(), 1, "one actual RST, no transport retransmission");
        assert_eq!(resets[0], (true, 0, 0, true, true));
    }
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}

#[test]
fn udp_actual_fput_releases_port_and_closed_observer_cannot_access_reused_handle() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    assert_eq!(counts(), (0, 0, 0));
    let endpoint = InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    endpoint.bind(address(43001)).unwrap();
    let observer = endpoint.clone();
    let handle = endpoint.udp_handle().unwrap();
    endpoint.close_description();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        replacement.udp_handle().unwrap(),
        handle,
        "same protocol slot makes stale access visible"
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
        observer.send_to(b"stale", Some(address(43003)), None),
        Err(SocketError::NotConnected)
    );
    let mut bytes = [0; 16];
    assert_eq!(
        observer.receive(&mut ReceiveBuffer::from_slice(&mut bytes), false),
        Err(SocketError::NotConnected)
    );
    assert_eq!(
        observer.set_reuse_address(true),
        Err(SocketError::NotConnected)
    );
    assert_eq!(observer.connection_result(), Err(SocketError::NotConnected));
    assert_eq!(observer.poll_state(), SocketPollState::closed());
    assert_eq!(observer.try_poll_state(), Some(SocketPollState::closed()));
    observer.close_description();
    drop(observer);
    assert_eq!(replacement.address().unwrap().port, 43001);
    assert_eq!(counts(), (1, 1, 0));
    replacement.close_description();
    assert_eq!(counts(), (0, 0, 0));
}
#[test]
fn closed_listener_observer_does_not_hold_port_or_retire_replacement() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let listener = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    listener.bind(address(43004)).unwrap();
    listener.listen(3).unwrap();
    assert_eq!(counts(), (3, 0, 1));
    let observer = listener.clone();
    listener.close_description();
    assert_eq!(counts(), (0, 0, 0));
    let replacement = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
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
        observer.accept(
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap()
        ),
        Err(SocketError::NotConnected)
    ));
    observer.close_description();
    drop((listener, observer));
    assert_eq!(replacement.address().unwrap().port, 43004);
    assert_eq!(counts(), (1, 0, 1));
    replacement.close_description();
}
#[test]
fn deferred_actual_close_releases_port_before_the_next_task_transaction() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
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
    assert_eq!(endpoint.try_poll_state(), Some(SocketPollState::closed()));
    assert_eq!(
        guard.endpoints.len(),
        1,
        "busy owner keeps command-owned slot until drain"
    );
    drop(guard);
    let replacement = InetSocket::new(
        InetProtocol::Udp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
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
                InetProtocol::Tcp,
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
            )
            .unwrap(),
        );
    }
    assert_eq!(
        counts(),
        (SOCKET_STORAGE_CAPACITY, 0, SOCKET_STORAGE_CAPACITY)
    );
    assert!(matches!(
        InetSocket::new(
            InetProtocol::Tcp,
            Pipe::notification_pair().unwrap(),
            Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
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
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
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
    use super::super::InternetOption;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let endpoint = InetSocket::new(
        InetProtocol::Tcp,
        Pipe::notification_pair().unwrap(),
        Arc::new(SocketTimeouts::new()),
        crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap(),
    )
    .unwrap();
    assert_eq!(endpoint.option(InternetOption::HopLimit).unwrap(), 64);
    endpoint.set_hop_limit(Some(29)).unwrap();
    endpoint.set_no_delay(true).unwrap();
    endpoint.set_max_segment(88).unwrap();
    let peer = InetAddress {
        address: Ipv4Addr::new(10, 0, 2, 16),
        port: 44000,
    };
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
            network
                .sockets
                .get::<smoltcp::socket::tcp::Socket<'static>>(
                    network.tcp_endpoints[&id].first_handle_for_test()
                )
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
        let listener = scoped_endpoint(&crate::net::initial_net(), InetProtocol::Tcp);
        listener.bind(address(port)).unwrap();
        listener.listen(1).unwrap();
        ingress(peer_arp_frame());
        ingress(peer_tcp_frame(port, TcpControl::Syn, None));
        let ack = syn_ack(port);
        ingress(peer_tcp_frame(port, TcpControl::None, Some(ack)));
        if missing_route {
            configure_up(&crate::net::initial_net(), 2, false).unwrap();
        } else {
            crate::allocation_failure::fail_next();
        }
        listener.close_description();
        if !missing_route {
            assert!(crate::allocation_failure::was_consumed(), "actual reset packet preparation must fail, not a synthetic error");
        }
        assert_eq!(counts(), (0, 0, 0), "best-effort reset cannot retain transport after no route or OOM");
        let replacement = scoped_endpoint(&crate::net::initial_net(), InetProtocol::Tcp);
        replacement.bind(address(port)).unwrap();
        replacement.close_description();
        configure_up(&crate::net::initial_net(), 2, true).unwrap();
    }
}

#[test]
fn actual_fin_wait2_linger_is_orphan_only_and_expires_without_route_or_interface() {
    use smoltcp::{socket::tcp::State, time::Duration, wire::*};
    use super::super::InternetOption;
    let _serial = SERIAL.lock().unwrap();
    for (ordinal, seconds) in [-1, 1, 0, 120].into_iter().enumerate() {
        initialize();
        let port = 43040 + ordinal as u16;
        let listener = scoped_endpoint(&crate::net::initial_net(), InetProtocol::Tcp);
        listener.bind(address(port)).unwrap();
        listener.set_timer_option(InternetOption::Linger2, seconds).unwrap();
        listener.listen(1).unwrap();
        ingress(peer_arp_frame());
        ingress(peer_tcp_frame(port, TcpControl::Syn, None));
        let ack = syn_ack(port);
        ingress(peer_tcp_frame(port, TcpControl::None, Some(ack)));
        let child = listener.accept(Pipe::notification_pair().unwrap(), Arc::new(SocketTimeouts::new()),
            crate::net::SocketMemory::try_new(crate::net::initial_net()).unwrap()).unwrap();
        listener.close_description();
        let InetEndpoint::Tcp(id) = child.endpoint else { unreachable!() };
        child.shutdown(1).unwrap();
        initial_stack().unwrap().lock().unwrap().poll();
        let fin_ack = {
            let wire = crate::devices::model::WIRE.lock().unwrap();
            wire.1.iter().find_map(|frame| {
                let ethernet = EthernetFrame::new_checked(frame.as_slice()).ok()?;
                if ethernet.ethertype() != EthernetProtocol::Ipv4 { return None; }
                let ip = Ipv4Packet::new_checked(ethernet.payload()).ok()?;
                if ip.next_header() != IpProtocol::Tcp { return None; }
                let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
                (tcp.src_port() == port && tcp.fin()).then_some(tcp.seq_number() + 1)
            }).expect("actual protocol must publish FIN")
        };
        ingress(peer_tcp_frame(port, TcpControl::None, Some(fin_ack)));
        {
            let network = initial_stack().unwrap().lock().unwrap();
            let tcp = network.sockets.get::<smoltcp::socket::tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test());
            assert_eq!(tcp.state(), State::FinWait2);
            assert_eq!(tcp.maintenance_deadline(), None, "live SHUT_WR cannot acquire orphan FIN deadline");
        }
        child.close_description();
        if seconds < 0 {
            assert_eq!(counts(), (0, 0, 0), "negative LINGER2 retires actual connection at final fput");
            initial_stack().unwrap().lock().unwrap().poll();
        } else {
            let deadline = {
                let network = initial_stack().unwrap().lock().unwrap();
                network.sockets.get::<smoltcp::socket::tcp::Socket>(network.tcp_endpoints[&id].first_handle_for_test())
                    .maintenance_deadline().unwrap()
            };
            let before = now();
            let expected_seconds = if seconds == 0 { 60 } else { seconds as u64 };
            assert!(deadline > before + Duration::from_millis(expected_seconds * 1000 - 100));
            configure_up(&crate::net::initial_net(), 2, false).unwrap();
            initial_stack().unwrap().lock().unwrap().poll_at(deadline - Duration::from_millis(1));
            assert_eq!(counts(), (1, 0, 1));
            initial_stack().unwrap().lock().unwrap().poll_at(deadline);
            assert_eq!(counts(), (0, 0, 0), "FIN_WAIT2 clock cannot depend on route or NIC");
            configure_up(&crate::net::initial_net(), 2, true).unwrap();
        }
        let replacement = scoped_endpoint(&crate::net::initial_net(), InetProtocol::Tcp);
        replacement.bind(address(port)).unwrap();
        replacement.close_description();
    }
}
