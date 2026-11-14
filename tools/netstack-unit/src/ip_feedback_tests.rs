// 用实际网卡引用覆盖队列顺序、SO_ERROR 独立消费及接收截断。
#[test]
fn dual_stack_error_queue_keeps_quote_metadata_and_rearms_pending_error() {
    use network_protocol::{IcmpErrorExtension, InternetErrorOrigin, InternetPacketInfo};
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    learn(&net, &device, 2002);
    for ipv6 in [false, true] {
        let endpoint = socket(&net, ipv6, false);
        endpoint.set_receive_error(ipv6, true).unwrap();
        let target = if ipv6 {
            InetAddress::V6(core::net::SocketAddrV6::new(PEER6, 44333, 0, 2))
        } else {
            InetAddress::new(PEER4.into(), 44333)
        };
        let payload = b"visible original payload";
        endpoint.send_to(payload, Some(target), &[]).unwrap();
        poll(&net, 2200);
        let output = frames(&device);
        let quote = EthernetFrame::parse(&output[0]).unwrap().payload();
        for port_unreachable in [false, true] {
            let mut error = vec![0; (if ipv6 { 40 } else { 20 }) + 8 + quote.len()];
            if ipv6 {
                let (kind, code) = if port_unreachable { (1, 4) } else { (3, 0) };
                packet_wire::icmpv6::emit_error(
                    &mut error[40..],
                    kind,
                    code,
                    0,
                    (PEER6, LOCAL6),
                    quote,
                    &[],
                )
                .unwrap();
                Ipv6Header {
                    source: PEER6,
                    destination: LOCAL6,
                    next_header: 58,
                    hop_limit: 64,
                    traffic_class: 0,
                    flow_label: 0,
                }
                .emit(&mut error)
                .unwrap();
            } else {
                let (kind, code) = if port_unreachable { (3, 3) } else { (11, 0) };
                packet_wire::icmpv4::emit_error(&mut error[20..], kind, code, 0, quote, &[])
                    .unwrap();
                Ipv4Header {
                    source: PEER4,
                    destination: LOCAL4,
                    protocol: 1,
                    hop_limit: 64,
                    dscp_ecn: 0,
                    identification: 0,
                    fragment: 0x4000,
                    options: &[],
                }
                .emit(&mut error)
                .unwrap();
            }
            input(&device, envelope(&error));
        }
        poll(&net, 2201);
        assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionRefused));
        assert!(endpoint.poll_state().error);
        let mut short = crate::receive::ReceiveBuffer::try_new(4).unwrap();
        let (length, error) = endpoint.receive_error(&mut short).unwrap();
        assert_eq!(short.into_bytes(), &payload[..4]);
        assert_eq!(length, payload.len());
        assert_eq!(error.error, SocketError::HostUnreachable);
        assert_eq!(error.destination, target);
        assert_eq!(error.offender.unwrap().ip(), target.ip());
        assert_eq!(
            error.origin,
            if ipv6 {
                InternetErrorOrigin::Icmpv6
            } else {
                InternetErrorOrigin::Icmpv4
            }
        );
        assert_eq!(endpoint.take_error(), Some(SocketError::ConnectionRefused));
        let mut empty = crate::receive::ReceiveBuffer::try_new(0).unwrap();
        assert_eq!(
            endpoint.receive_error(&mut empty).unwrap().1.error,
            SocketError::ConnectionRefused
        );
        assert_eq!(endpoint.receive_error(&mut empty), Err(SocketError::Again));
        assert!(!endpoint.poll_state().error);

        let (ip_length, option, packet_info) = if ipv6 {
            endpoint
                .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
                .unwrap();
            (
                40,
                InternetOption::Ipv6ReceiveErrorExtensions,
                InternetPacketInfo::Ipv6 {
                    interface_index: 2,
                    destination: LOCAL6,
                },
            )
        } else {
            endpoint.set_packet_info(true).unwrap();
            (
                20,
                InternetOption::Ipv4ReceiveErrorExtensions,
                InternetPacketInfo::Ipv4 {
                    interface_index: 2,
                    specific_address: LOCAL4,
                    destination: LOCAL4,
                },
            )
        };
        let objects = [packet_wire::icmp::Object {
            class: 200,
            subtype: 1,
            data: &[1, 2, 3, 4],
        }];
        let mut multipart = vec![0; ip_length + 8 + 128 + 12];
        if ipv6 {
            packet_wire::icmpv6::emit_error(
                &mut multipart[40..],
                1,
                4,
                0,
                (PEER6, LOCAL6),
                quote,
                &objects,
            )
            .unwrap();
            Ipv6Header {
                source: PEER6,
                destination: LOCAL6,
                next_header: 58,
                hop_limit: 64,
                traffic_class: 0,
                flow_label: 0,
            }
            .emit(&mut multipart)
            .unwrap();
        } else {
            packet_wire::icmpv4::emit_error(&mut multipart[20..], 3, 3, 0, quote, &objects)
                .unwrap();
            Ipv4Header {
                source: PEER4,
                destination: LOCAL4,
                protocol: 1,
                hop_limit: 64,
                dscp_ecn: 0,
                identification: 0,
                fragment: 0x4000,
                options: &[],
            }
            .emit(&mut multipart)
            .unwrap();
        }
        let offset = 128 - ip_length - 8;
        assert_eq!(endpoint.option(option), Ok(0));
        for (enabled, invalid) in [(false, false), (true, false), (true, true)] {
            endpoint
                .set_receive_error_extensions(ipv6, enabled)
                .unwrap();
            assert_eq!(endpoint.option(option), Ok(i32::from(enabled)));
            if invalid {
                // 损坏扩展 checksum，重新计算外层 checksum，错误本身仍应交付。
                *multipart.last_mut().unwrap() ^= 1;
                if ipv6 {
                    packet_wire::icmpv6::emit(&mut multipart[40..], 1, 4, PEER6, LOCAL6).unwrap();
                } else {
                    packet_wire::icmpv4::emit(&mut multipart[20..], 3, 3).unwrap();
                }
            }
            input(&device, envelope(&multipart));
            poll(&net, 2201);
            // 扩展选项在入队时生效，读取时改选项不会改写原记录。
            endpoint
                .set_receive_error_extensions(ipv6, !enabled)
                .unwrap();
            let mut bytes = crate::receive::ReceiveBuffer::try_new(160).unwrap();
            let (length, error) = endpoint.receive_error(&mut bytes).unwrap();
            assert_eq!(error.error, SocketError::ConnectionRefused);
            assert_eq!(error.packet_info, Some(packet_info));
            assert_eq!(
                error.extension,
                enabled.then_some(IcmpErrorExtension {
                    offset: offset as u16,
                    invalid
                })
            );
            assert_eq!(length, offset + 12);
            let bytes = bytes.into_bytes();
            assert_eq!(&bytes[..payload.len()], payload);
            assert!(bytes[payload.len()..offset].iter().all(|&byte| byte == 0));
            assert_eq!(&bytes[offset..], &multipart[multipart.len() - 12..]);
        }

        if ipv6 {
            // 首片的声明长度仍是完整片长。ICMP 截断引用不要求可见载荷为 8 字节倍数。
            endpoint.send_to(&[0x29; 3000], Some(target), &[]).unwrap();
            poll(&net, 2202);
            let output = frames(&device);
            let quote = &EthernetFrame::parse(&output[0]).unwrap().payload()[..61];
            let mut ptb = vec![0; 48 + quote.len()];
            packet_wire::icmpv6::emit_error(
                &mut ptb[40..],
                2,
                0,
                1280,
                (PEER6, LOCAL6),
                quote,
                &[],
            )
            .unwrap();
            Ipv6Header {
                source: PEER6,
                destination: LOCAL6,
                next_header: 58,
                hop_limit: 64,
                traffic_class: 0,
                flow_label: 0,
            }
            .emit(&mut ptb)
            .unwrap();
            input(&device, envelope(&ptb));
            poll(&net, 2203);
            let mut bytes = crate::receive::ReceiveBuffer::try_new(8).unwrap();
            let (length, error) = endpoint.receive_error(&mut bytes).unwrap();
            assert_eq!(length, 5);
            assert_eq!(bytes.into_bytes(), [0x29; 5]);
            assert_eq!(error.info, 1280);
            assert_eq!(error.error, SocketError::MessageTooLarge);

            endpoint
                .set_ipv6_option(InternetOption::Ipv6Mtu, 1280)
                .unwrap();
            endpoint
                .set_ipv6_option(InternetOption::Ipv6DontFragment, 1)
                .unwrap();
        } else {
            endpoint.set_ipv4_mtu_discovery(2).unwrap();
        }
        assert_eq!(
            endpoint.send_to(&[0; 1500], Some(target), &[]),
            Err(SocketError::MessageTooLarge)
        );
        let (length, local) = endpoint.receive_error(&mut empty).unwrap();
        assert_eq!(length, 0);
        assert_eq!(local.origin, InternetErrorOrigin::Local);
        assert_eq!(local.offender, None);
        assert_eq!(local.packet_info, None);
        assert_eq!(local.extension, None);
        assert_eq!(local.info, if ipv6 { 1280 } else { 1500 });
        assert_eq!(endpoint.take_error(), None);
        endpoint.close_description();
    }
}

fn ipv4_feedback(
    quote: &[u8],
    (kind, code, parameter): (u8, u8, u32),
    objects: &[packet_wire::icmp::Object<'_>],
) -> Vec<u8> {
    let mut packet = vec![0; 20 + 8 + quote.len().max(128).next_multiple_of(4) + 16];
    let length =
        packet_wire::icmpv4::emit_error(&mut packet[20..], kind, code, parameter, quote, objects)
            .unwrap();
    packet.truncate(20 + length);
    Ipv4Header {
        source: PEER4,
        destination: LOCAL4,
        protocol: 1,
        hop_limit: 64,
        dscp_ecn: 0,
        identification: 0,
        fragment: 0x4000,
        options: &[],
    }
    .emit(&mut packet)
    .unwrap();
    envelope(&packet)
}

fn fragmentation_needed(quote: &[u8], mtu: u16) -> Vec<u8> {
    ipv4_feedback(quote, (3, 4, u32::from(mtu)), &[])
}

// ICMP 引用匹配、路径共享及设备实际重分段横跨多个 owner，格式用例无法证明。
#[test]
fn ipv4_pmtu_feedback_changes_udp_raw_and_inflight_tcp_output() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    learn(&net, &device, 2002);
    let target = InetAddress::new(PEER4.into(), 44333);
    let sender = socket(&net, false, false);
    assert_eq!(
        sender.option(InternetOption::Ipv4Mtu),
        Err(SocketError::NotConnected)
    );
    sender.connect(target).unwrap();
    sender.set_ipv4_mtu_discovery(2).unwrap();
    sender.send_to(&[0x6a; 1400], None, &[]).unwrap();
    poll(&net, 2100);
    let output = frames(&device);
    let quote = &output[0][14..42];
    let mut bad = quote.to_vec();
    bad[20] ^= 1;
    input(&device, fragmentation_needed(&bad, 1200));
    let mut bad = fragmentation_needed(quote, 1200);
    bad[36] ^= 1;
    input(&device, bad);
    input(&device, fragmentation_needed(quote, 67));
    poll(&net, 2101);
    assert_eq!(sender.option(InternetOption::Ipv4Mtu), Ok(1500));
    assert_eq!(sender.take_error(), None);

    input(&device, fragmentation_needed(quote, 1200));
    poll(&net, 2102);
    assert_eq!(sender.option(InternetOption::Ipv4Mtu), Ok(1200));
    assert!(sender.poll_state().error);
    assert_eq!(sender.take_error(), Some(SocketError::MessageTooLarge));
    assert_eq!(
        sender.send_to(&[0; 1173], None, &[]),
        Err(SocketError::MessageTooLarge)
    );
    sender.set_ipv4_mtu_discovery(3).unwrap();
    sender.send_to(&[0; 1173], None, &[]).unwrap();
    poll(&net, 2103);
    let probe = frames(&device);
    let ip = Ipv4Packet::parse(&probe[0][14..]).unwrap();
    assert_eq!(ip.bytes().len(), 1201);
    assert_eq!(ip.bytes()[6] & 0x40, 0x40);

    let raw = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Raw(253),
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    raw.connect(InetAddress::new(PEER4.into(), 0)).unwrap();
    raw.set_ipv4_mtu_discovery(2).unwrap();
    raw.set_receive_error(false, true).unwrap();
    assert_eq!(raw.option(InternetOption::Ipv4Mtu), Ok(1200));
    raw.send_to(&[0x43; 1180], None, &[]).unwrap();
    poll(&net, 2104);
    let output = frames(&device);
    let legacy_quote = &output[0][14..42];
    for time in [2105, 2106] {
        input(&device, fragmentation_needed(legacy_quote, 0));
        poll(&net, time);
        assert_eq!(raw.option(InternetOption::Ipv4Mtu), Ok(1006));
        assert_eq!(raw.take_error(), Some(SocketError::MessageTooLarge));
        let mut bytes = crate::receive::ReceiveBuffer::try_new(4).unwrap();
        let (length, error) = raw.receive_error(&mut bytes).unwrap();
        assert_eq!(length, 8);
        assert_eq!(bytes.into_bytes(), [0x43; 4]);
        assert_eq!(error.info, 1006);
    }
    raw.set_receive_error_extensions(false, true).unwrap();
    let objects = [packet_wire::icmp::Object {
        class: 200,
        subtype: 1,
        data: &[1, 2, 3, 4],
    }];
    for (hdrincl, offset) in [(false, 108), (true, 128)] {
        raw.set_header_included(hdrincl).unwrap();
        input(&device, ipv4_feedback(legacy_quote, (11, 0, 0), &objects));
        poll(&net, 2106);
        let mut short = crate::receive::ReceiveBuffer::try_new(4).unwrap();
        let (length, error) = raw.receive_error(&mut short).unwrap();
        assert_eq!(error.extension.unwrap().offset, offset);
        assert_eq!(length, usize::from(offset) + 12);
        assert_eq!(
            short.into_bytes(),
            if hdrincl {
                &legacy_quote[..4]
            } else {
                &[0x43; 4]
            }
        );
    }
    assert_eq!(sender.option(InternetOption::Ipv4Mtu), Ok(1006));

    // 另一目的的 TCP 路径不继承上面的 UDP/RAW 估计。
    let tcp_peer = Ipv4Addr::new(10, 0, 2, 16);
    input(&device, peer_arp_frame_for(LOCAL4, tcp_peer));
    let listener = InetSocket::new(
        network_protocol::InetFamily::Ipv4,
        InetProtocol::Tcp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap();
    listener
        .bind(InetAddress::new(LOCAL4.into(), 44340))
        .unwrap();
    listener.listen(1).unwrap();
    use smoltcp::wire::{TcpControl, TcpSeqNumber};
    input(&device, peer_tcp_frame(44340, TcpControl::Syn, None));
    poll(&net, 2107);
    let syn_ack = frames(&device)
        .iter()
        .find_map(|frame| {
            let ip = Ipv4Packet::parse(frame.get(14..)?).ok()?;
            let tcp =
                packet_wire::tcp::TcpPacket::parse(ip.payload(), LOCAL4.into(), tcp_peer.into())
                    .ok()?;
            Some(tcp.header().sequence().wrapping_add(1))
        })
        .unwrap();
    input(
        &device,
        peer_tcp_frame(44340, TcpControl::None, Some(TcpSeqNumber(syn_ack as i32))),
    );
    poll(&net, 2108);
    frames(&device);
    let child = listener.accept(Arc::new(crate::Event::default())).unwrap();
    child.set_no_delay(true).unwrap();
    assert_eq!(child.option(InternetOption::Ipv4Mtu), Ok(1500));
    let payload = [0xa8; 500];
    child.send_to(&payload, None, &[]).unwrap();
    poll(&net, 2109);
    let output = frames(&device);
    let quote = &output[0][14..42];
    let mut wrong_sequence = quote.to_vec();
    wrong_sequence[24..28].copy_from_slice(&syn_ack.wrapping_add(500).to_be_bytes());
    input(&device, fragmentation_needed(&wrong_sequence, 296));
    poll(&net, 2110);
    assert_eq!(child.option(InternetOption::Ipv4Mtu), Ok(1500));
    input(&device, fragmentation_needed(quote, 296));
    poll(&net, 2111);
    assert_eq!(child.option(InternetOption::Ipv4Mtu), Ok(296));
    let mut retransmitted = Vec::new();
    for frame in frames(&device) {
        let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
        assert!(ip.bytes().len() <= 296);
        let segment =
            packet_wire::tcp::TcpPacket::parse(ip.payload(), LOCAL4.into(), tcp_peer.into())
                .unwrap();
        retransmitted.extend_from_slice(segment.payload());
    }
    assert_eq!(retransmitted, payload);
    child.close_description();
    listener.close_description();
    raw.close_description();
    sender.close_description();
}
