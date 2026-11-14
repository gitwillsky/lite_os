// 实际 NET/NIC 路径验证密钥策略、握手、坏签名静默丢弃与 child 所有权；纯 MAC 向量覆盖不了这些边界。
mod tcp_authentication {
    use super::*;
    use core::net::IpAddr;
    use packet_wire::{
        ipv4::{Ipv4Header, Ipv4Packet},
        tcp::TcpPacket,
    };
    use smoltcp::wire::{TcpControl, TcpSeqNumber};

    fn sign(frame: Vec<u8>, key: &::tcp::Md5Key) -> Vec<u8> {
        protect(
            frame,
            ::tcp::Authentication::Md5,
            ::tcp::SigningKey::Md5(key),
        )
    }

    fn protect(frame: Vec<u8>, mode: ::tcp::Authentication, key: ::tcp::SigningKey<'_>) -> Vec<u8> {
        let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
        let mut segment = ::tcp::Segment::capture(
            TcpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into()).unwrap(),
        )
        .unwrap();
        segment.authentication = mode;
        let mut output = vec![0; 34 + segment.buffer_len()];
        output[..14].copy_from_slice(&frame[..14]);
        segment
            .emit_with_authentication(
                &mut output[34..],
                (ip.source().into(), ip.destination().into()),
                key,
                false,
            )
            .unwrap();
        Ipv4Header {
            source: ip.source(),
            destination: ip.destination(),
            protocol: 6,
            hop_limit: 64,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0x4000,
            options: &[],
        }
        .emit(&mut output[14..])
        .unwrap();
        output
    }

    fn ao_sign(frame: Vec<u8>, key: &::tcp::AoTrafficKey, ids: (u8, u8)) -> Vec<u8> {
        protect(
            frame,
            ::tcp::Authentication::Ao,
            ::tcp::SigningKey::Ao { key, ids, sne: 0 },
        )
    }

    fn ao_packet(frame: &[u8]) -> ((IpAddr, IpAddr), ::tcp::IncomingSegment<'_>) {
        let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
        let addresses = (ip.source().into(), ip.destination().into());
        let packet = TcpPacket::parse(ip.payload(), addresses.0, addresses.1).unwrap();
        (addresses, ::tcp::IncomingSegment::capture(packet).unwrap())
    }
    fn tcp_frames() -> Vec<Vec<u8>> {
        core::mem::take(&mut WIRE.lock().unwrap().1)
            .into_iter()
            .filter(|frame| frame.len() >= 34 && frame[12..14] == [8, 0] && frame[23] == 6)
            .collect()
    }
    #[track_caller]
    fn signed_sequence(frames: &[Vec<u8>], key: &::tcp::Md5Key, port: u16) -> TcpSeqNumber {
        let mut sequence = None;
        for frame in frames {
            let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
            let packet =
                TcpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into())
                    .unwrap();
            if packet.header().source_port() != port {
                continue;
            }
            assert!(
                key.verify(packet, ip.source().into(), ip.destination().into()),
                "every actual reply must use its selected key"
            );
            sequence = Some(TcpSeqNumber(packet.header().sequence() as i32));
        }
        sequence.expect("real output for the requested TCP endpoint was required")
    }
    #[test]
    fn md5_native_handshake_rejects_bad_input_and_listener_rekey_keeps_its_established_child() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let peer = Ipv4Addr::new(10, 0, 2, 16);
        let selector = network_protocol::TcpKeySelector::host(peer.into(), Some(2));
        let key = ::tcp::Md5Key::try_new(b"first-secret").unwrap();
        let wrong = ::tcp::Md5Key::try_new(b"wrong-secret").unwrap();
        let listener = scoped_endpoint(namespace(), InetProtocol::Tcp);
        listener.bind_to_device(b"eth0", false).unwrap();
        listener.bind(address(43070)).unwrap();
        listener.set_md5_key(selector, b"first-secret").unwrap();
        listener.listen(2).unwrap();
        ingress(peer_arp_frame());
        tcp_frames();
        ingress(peer_tcp_frame(43070, TcpControl::Syn, None));
        assert!(tcp_frames().is_empty());
        ingress(sign(peer_tcp_frame(43070, TcpControl::Syn, None), &wrong));
        assert!(tcp_frames().is_empty() && !listener.poll_state().readable);
        crate::allocation_failure::fail_next();
        assert_eq!(
            listener.set_md5_key(selector, b"replacement"),
            Err(SocketError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        ingress(sign(peer_tcp_frame(43070, TcpControl::Syn, None), &key));
        let ack = signed_sequence(&tcp_frames(), &key, 43070) + 1;
        ingress(sign(
            peer_tcp_frame(43070, TcpControl::None, Some(ack)),
            &key,
        ));
        assert!(listener.poll_state().readable);
        listener.set_md5_key(selector, b"second-secret").unwrap();
        let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
        assert_eq!(child.option(InternetOption::MaxSegment), Ok(516));
        ingress(sign(
            peer_tcp_payload(43070, TcpControl::None, Some(ack), 1001, b"kept"),
            &key,
        ));
        let mut bytes = [0; 4];
        assert_eq!(receive(&child, &mut bytes, false).unwrap().0, 4);
        assert_eq!(&bytes, b"kept");
        child.send_to(&[7; 1032], None, &[]).unwrap();
        initial_stack().unwrap().lock().unwrap().poll();
        let frames = tcp_frames();
        signed_sequence(&frames, &key, 43070);
        let payloads: Vec<_> = frames
            .iter()
            .filter_map(|frame| {
                let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
                let packet =
                    TcpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into())
                        .unwrap();
                (packet.header().source_port() == 43070 && !packet.payload().is_empty())
                    .then_some(packet.payload().len())
            })
            .collect();
        assert_eq!(
            payloads,
            [516, 516],
            "TCP_MAXSEG must report actual signed data capacity"
        );
        listener.set_md5_key(selector, &[]).unwrap();
        assert_eq!(
            listener.set_md5_key(selector, &[]),
            Err(SocketError::NoEntry)
        );
        // 真实对端关闭测试连接，不能把仍在等待 FIN/ACK 的孤儿带入其他共享 NET 用例。
        ingress(sign(
            peer_tcp_payload(43070, TcpControl::Rst, None, 1005, &[]),
            &key,
        ));
        child.close_description();
        listener.close_description();

        let active = scoped_endpoint(namespace(), InetProtocol::Tcp);
        active.bind_to_device(b"eth0", false).unwrap();
        active.set_md5_key(selector, b"first-secret").unwrap();
        assert_eq!(
            active.connect(InetAddress::new(peer.into(), 44000)),
            Err(SocketError::InProgress)
        );
        let port = active.address().unwrap().port();
        initial_stack().unwrap().lock().unwrap().poll();
        let ack = signed_sequence(&tcp_frames(), &key, port) + 1;
        ingress(sign(peer_tcp_frame(port, TcpControl::Syn, Some(ack)), &key));
        active.connection_result().unwrap();
        assert_eq!(active.option(InternetOption::MaxSegment), Ok(516));
        signed_sequence(&tcp_frames(), &key, port);
        ingress(sign(peer_tcp_frame(port, TcpControl::Rst, None), &wrong));
        assert_eq!(
            active.connection_result(),
            Ok(()),
            "forged RST must not close a protected connection"
        );
        ingress(sign(peer_tcp_frame(port, TcpControl::Rst, None), &key));
        assert_eq!(active.take_error(), Some(SocketError::ConnectionReset));
        active.close_description();
    }

    #[test]
    fn ao_native_handshakes_and_peer_requested_rotation_keep_signed_data_path() {
        use core::net::SocketAddr;
        use network_protocol::{
            TcpAoAlgorithm, TcpAoCounters, TcpAoKey, TcpAoSelection, TcpKeySelector,
        };
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let peer = SocketAddr::new(Ipv4Addr::new(10, 0, 2, 16).into(), 44000);
        let local = SocketAddr::new(Ipv4Addr::new(10, 0, 2, 15).into(), 43071);
        let first = TcpAoKey {
            selector: TcpKeySelector::host(peer.ip(), Some(2)),
            algorithm: TcpAoAlgorithm::HmacSha1,
            send_id: 11,
            receive_id: 21,
            cover_options: true,
        };
        let second = TcpAoKey {
            algorithm: TcpAoAlgorithm::Aes128Cmac,
            send_id: 12,
            receive_id: 22,
            ..first
        };
        let first_master =
            ::tcp::AoMasterKey::new(::tcp::AoAlgorithm::HmacSha1, b"ao-first-secret", true)
                .unwrap();
        let second_master =
            ::tcp::AoMasterKey::new(::tcp::AoAlgorithm::Aes128Cmac, b"ao-next-secret", true)
                .unwrap();
        let listener = scoped_endpoint(namespace(), InetProtocol::Tcp);
        listener.bind_to_device(b"eth0", false).unwrap();
        listener.bind(address(local.port())).unwrap();
        let unrelated = TcpAoKey {
            selector: TcpKeySelector::host(Ipv4Addr::new(10, 0, 2, 17).into(), Some(2)),
            ..first
        };
        listener
            .replace_ao_keys(&[(unrelated, b"other-peer")], TcpAoSelection::default())
            .unwrap();
        listener
            .replace_ao_keys(
                &[(first, b"ao-first-secret"), (second, b"ao-next-secret")],
                TcpAoSelection::default(),
            )
            .unwrap();
        assert_eq!(
            listener.set_md5_key(unrelated.selector, b"overlap"),
            Err(SocketError::AddressInUse)
        );
        listener.listen(2).unwrap();
        ingress(peer_arp_frame());
        tcp_frames();

        let peer_isn = 1000;
        let syn = first_master.derive((peer, peer_isn), (local, 0)).unwrap();
        ingress(peer_tcp_frame(local.port(), TcpControl::Syn, None));
        ingress(ao_sign(
            peer_tcp_frame(local.port(), TcpControl::Syn, None),
            &syn,
            (22, 11),
        ));
        assert!(tcp_frames().is_empty());
        ingress(ao_sign(
            peer_tcp_frame(local.port(), TcpControl::Syn, None),
            &syn,
            (21, 11),
        ));
        let reply = tcp_frames();
        let (addresses, syn_ack) = ao_packet(&reply[0]);
        let local_isn = syn_ack.sequence().0.0 as u32;
        let send_first = first_master
            .derive((local, local_isn), (peer, peer_isn))
            .unwrap();
        assert!(syn_ack.authenticate_ao(addresses, &send_first, 0).is_some());
        assert_eq!(syn_ack.ao_ids(), Some((11, 21)));

        // The listener can retire keys during the half-open phase, while the actual child
        // keeps both configured keys and can follow the peer's authenticated RNext request.
        listener
            .set_ao_key(first, &[], TcpAoSelection::default())
            .unwrap();
        listener
            .set_ao_key(second, &[], TcpAoSelection::default())
            .unwrap();
        let ack = TcpSeqNumber(local_isn.wrapping_add(1) as i32);
        let receive_first = first_master
            .derive((peer, peer_isn), (local, local_isn))
            .unwrap();
        ingress(ao_sign(
            peer_tcp_frame(local.port(), TcpControl::None, Some(ack)),
            &receive_first,
            (21, 11),
        ));
        let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
        child.set_md5_key(unrelated.selector, b"isolated").unwrap();
        let listener_counters = listener.ao_status().unwrap().counters;
        assert_eq!(
            (listener_counters.unsigned, listener_counters.bad_mac),
            (1, 1)
        );
        let receive_second = second_master
            .derive((peer, peer_isn), (local, local_isn))
            .unwrap();
        let data = peer_tcp_payload(local.port(), TcpControl::None, Some(ack), 1001, b"ao");
        ingress(ao_sign(data.clone(), &receive_first, (22, 12)));
        assert!(!child.poll_state().readable);
        ingress(ao_sign(data, &receive_second, (22, 12)));
        let mut bytes = [0; 2];
        assert_eq!(receive(&child, &mut bytes, false).unwrap().0, 2);
        assert_eq!(&bytes, b"ao");
        assert_eq!(
            child.select_ao_keys(TcpAoSelection {
                current: None,
                receive: Some(99)
            }),
            Err(SocketError::NoEntry)
        );
        child
            .select_ao_keys(TcpAoSelection {
                current: None,
                receive: Some(22),
            })
            .unwrap();
        let payload = [0x5a; 100];
        child.send_to(&payload, None, &[]).unwrap();
        initial_stack().unwrap().lock().unwrap().poll();
        let reply = tcp_frames();
        let send_second = second_master
            .derive((local, local_isn), (peer, peer_isn))
            .unwrap();
        let sent = reply
            .iter()
            .find(|frame| {
                let (addresses, segment) = ao_packet(frame);
                segment.ao_ids() == Some((12, 22))
                    && segment
                        .authenticate_ao(addresses, &send_second, 0)
                        .is_some_and(|segment| segment.payload == payload)
            })
            .unwrap();

        let mut error = vec![0; 14 + 20 + 8 + 28];
        let incoming = peer_tcp_frame(local.port(), TcpControl::None, None);
        error[..14].copy_from_slice(&incoming[..14]);
        packet_wire::icmpv4::emit_error(&mut error[34..], 3, 4, 100, &sent[14..42], &[]).unwrap();
        let ip = Ipv4Packet::parse(&incoming[14..]).unwrap();
        Ipv4Header {
            source: ip.source(),
            destination: ip.destination(),
            protocol: 1,
            hop_limit: 64,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0,
            options: &[],
        }
        .emit(&mut error[14..])
        .unwrap();
        ingress(error);
        assert_eq!(child.option(InternetOption::Ipv4Mtu), Ok(1500));
        let status = child.ao_status().unwrap();
        assert_eq!(
            status.counters,
            TcpAoCounters {
                verified: 1,
                bad_mac: 1,
                ignored_icmp: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            child.set_ao_key(second, &[], TcpAoSelection::default()),
            Err(SocketError::Busy)
        );
        assert_eq!(
            child.set_ao_key(
                second,
                &[],
                TcpAoSelection {
                    current: Some(99),
                    receive: Some(21)
                }
            ),
            Err(SocketError::NoEntry)
        );
        assert_eq!(child.ao_status().unwrap(), status);
        child
            .set_ao_key(first, &[], TcpAoSelection::default())
            .unwrap();
        let next = TcpAoSelection {
            current: Some(11),
            receive: Some(21),
        };
        child
            .set_ao_key(first, b"ao-first-secret", TcpAoSelection::default())
            .unwrap();
        assert_eq!(
            child.replace_ao_keys(&[(first, b"ao-first-secret")], TcpAoSelection::default()),
            Err(SocketError::Busy)
        );
        assert_eq!(child.ao_status().unwrap(), status);
        child
            .replace_ao_keys(&[(first, b"ao-first-secret")], next)
            .unwrap();
        assert_eq!(child.ao_status().unwrap().selection, next);
        let reset = peer_tcp_payload(local.port(), TcpControl::Rst, None, 1003, &[]);
        ingress(ao_sign(reset.clone(), &receive_second, (22, 12)));
        assert_eq!(child.ao_status().unwrap().counters.key_not_found, 1);
        assert_eq!(child.take_error(), None);
        ingress(ao_sign(reset, &receive_first, (21, 11)));
        assert_eq!(child.take_error(), Some(SocketError::ConnectionReset));
        child.close_description();
        listener.close_description();
        tcp_frames();

        let active = scoped_endpoint(namespace(), InetProtocol::Tcp);
        active.bind_to_device(b"eth0", false).unwrap();
        active
            .set_ao_key(first, b"ao-first-secret", TcpAoSelection::default())
            .unwrap();
        assert_eq!(
            active.connect(InetAddress::new(peer.ip(), peer.port())),
            Err(SocketError::InProgress)
        );
        let local = SocketAddr::new(local.ip(), active.address().unwrap().port());
        initial_stack().unwrap().lock().unwrap().poll();
        let reply = tcp_frames();
        let (addresses, syn) = ao_packet(&reply[0]);
        let local_isn = syn.sequence().0.0 as u32;
        let send_syn = first_master.derive((local, local_isn), (peer, 0)).unwrap();
        assert!(syn.authenticate_ao(addresses, &send_syn, 0).is_some());
        let receive = first_master
            .derive((peer, peer_isn), (local, local_isn))
            .unwrap();
        let ack = TcpSeqNumber(local_isn.wrapping_add(1) as i32);
        ingress(ao_sign(
            peer_tcp_frame(local.port(), TcpControl::Syn, Some(ack)),
            &receive,
            (21, 11),
        ));
        active.connection_result().unwrap();
        let send = first_master
            .derive((local, local_isn), (peer, peer_isn))
            .unwrap();
        let reply = tcp_frames();
        let (addresses, ack) = ao_packet(&reply[0]);
        assert!(ack.authenticate_ao(addresses, &send, 0).is_some());
        active.disconnect().unwrap();
        initial_stack().unwrap().lock().unwrap().poll();
        assert!(tcp_frames().iter().any(|frame| {
            let (addresses, segment) = ao_packet(frame);
            segment.control() == ::tcp::Control::Rst
                && segment.authenticate_ao(addresses, &send, 0).is_some()
        }));
        active.close_description();
    }
}
