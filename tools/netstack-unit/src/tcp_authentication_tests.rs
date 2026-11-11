// 实际 NET/NIC 路径验证密钥策略、握手、坏签名静默丢弃与 child 所有权；纯 MAC 向量覆盖不了这些边界。
mod tcp_authentication {
    use super::*;
    use packet_wire::{
        ipv4::{Ipv4Header, Ipv4Packet},
        tcp::TcpPacket,
    };
    use smoltcp::wire::{TcpControl, TcpSeqNumber};

    fn sign(frame: Vec<u8>, key: &::tcp::Md5Key) -> Vec<u8> {
        let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
        let mut segment = ::tcp::Segment::capture(
            TcpPacket::parse(ip.payload(), ip.source().into(), ip.destination().into()).unwrap(),
        )
        .unwrap();
        segment.md5 = true;
        let mut output = vec![0; 34 + segment.buffer_len()];
        output[..14].copy_from_slice(&frame[..14]);
        segment
            .emit_md5(
                &mut output[34..],
                ip.source().into(),
                ip.destination().into(),
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
        let selector = network_protocol::TcpMd5Selector::host(peer.into(), Some(2));
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
}
