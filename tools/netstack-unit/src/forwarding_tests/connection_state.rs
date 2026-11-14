use super::super::ipv4_forwarding;
use super::*;
use core::net::IpAddr;
use packet_wire::ipv4::Ipv4Packet;

#[test]
fn stateful_rules_follow_dual_stack_transmit_and_idle_expiry() {
    use core::sync::atomic::Ordering::Relaxed;
    use packet_wire::{
        ipv4::Ipv4Header,
        tcp::{self, TcpHeader},
    };
    use router_protocol::connections::{Cursor, Identity, PAGE_LIMIT, Page, Phase, Protocol};
    use router_protocol::connections::changes::{Batch, Change, Cursor as ChangeCursor};

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, a, b) = routing_domain(89408);
    let connections = |cursor, now| {
        let page = crate::inet::stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .connections_at(cursor, Instant::from_millis(now))
            .unwrap();
        assert_eq!(Page::parse(&page.encode().unwrap(), cursor).unwrap(), page);
        page
    };
    let first_page = Cursor::default();
    let changes = |cursor| {
        let batch = crate::inet::stack(&net).unwrap().lock().unwrap().connection_changes_after(cursor).unwrap();
        assert_eq!(Batch::parse(&batch.encode().unwrap(), cursor).unwrap(), batch);
        batch
    };
    let addresses = [A, B].map(|address| {
        [router_protocol::configuration::Ipv6Address {
            address,
            prefix_length: 64,
            on_link: true,
        }]
    });
    let mut plans = [interface_plan(2, 40), interface_plan(3, 41)];
    for (plan, address) in plans.iter_mut().zip(&addresses) {
        plan.ipv6 = Some(router_protocol::interface::Ipv6Addresses::new(address).unwrap());
    }
    let interfaces = prepare_configuration(&net, plans.into_iter(), None).unwrap();
    interfaces.activate().unwrap();
    interfaces.confirm().unwrap();
    for now in [0, 1000, 2000, 2001] {
        poll(&net, now);
    }
    for (nic, local, peer, mac, subnet) in [(&a, A, FROM, FROM_MAC, 40), (&b, B, TO, TO_MAC, 41)] {
        inject(
            nic,
            neighbor(
                NdMessage::NeighborSolicitation { target: local },
                peer,
                local,
                mac,
            ),
        );
        inject(
            nic,
            peer_arp_frame_for(
                Ipv4Addr::new(10, subnet, 0, 15),
                Ipv4Addr::new(10, subnet, 0, 9),
            ),
        );
    }
    poll(&net, 2002);
    frames(&a);
    frames(&b);

    let configuration = router_protocol::configuration::Configuration::decode(br#"{
        "version": 1,
        "interfaces": {"a": {"device": {"kind": "name", "value": "a"}}},
        "firewall": {"prerouting": [
            {"destination": "0.0.0.0", "prefix_length": 0, "connection_state": "established", "action": {"kind": "accept"}},
            {"destination": "::", "prefix_length": 0, "connection_state": "established", "action": {"kind": "accept"}},
            {"destination": "0.0.0.0", "prefix_length": 0, "connection_state": "related", "action": {"kind": "accept"}},
            {"destination": "::", "prefix_length": 0, "connection_state": "related", "action": {"kind": "accept"}},
            {"ingress": "a", "destination": "0.0.0.0", "prefix_length": 0, "connection_state": "new", "action": {"kind": "accept"}},
            {"ingress": "a", "destination": "::", "prefix_length": 0, "connection_state": "new", "action": {"kind": "accept"}},
            {"destination": "0.0.0.0", "prefix_length": 0, "action": {"kind": "drop"}},
            {"destination": "::", "prefix_length": 0, "action": {"kind": "drop"}}
        ]}
    }"#).unwrap();
    let body = configuration.firewall_source(|_| Ok(Some(2))).unwrap();
    let batch = network_protocol::ConfigurationBatch::default()
        .with_prerouting(router_protocol::firewall::Prerouting::parse(&body).unwrap());
    let rules = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
    rules.activate().unwrap();
    rules.confirm().unwrap();

    let peers: [(IpAddr, IpAddr); 2] = [
        (
            Ipv4Addr::new(10, 40, 0, 9).into(),
            Ipv4Addr::new(10, 41, 0, 9).into(),
        ),
        (FROM.into(), TO.into()),
    ];
    let snapshot = stack(&net).unwrap().receive_snapshot().unwrap();
    assert!(peers.iter().all(|&(_, destination)| snapshot.prepare_for(destination).verify_transport));
    a.wire.lock().unwrap().2.receive_snapshot = Some((2, snapshot.clone()));
    b.wire.lock().unwrap().2.receive_snapshot = Some((3, snapshot));
    let ip_frame = |source, destination, protocol, payload: &[u8]| match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let mut bytes = ipv4_forwarding::datagram(64, 0, &[], payload);
            Ipv4Header {
                source,
                destination,
                protocol,
                hop_limit: 64,
                dscp_ecn: 0,
                identification: 0,
                fragment: 0,
                options: &[],
            }
            .emit(&mut bytes[14..])
            .unwrap();
            bytes
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            frame(source, destination, 64, protocol, payload)
        }
        _ => unreachable!(),
    };
    let inject_pair = |nic, reversed, ports: (u16, u16), corrupt| {
        for (left, right) in peers {
            let (source, destination) = if reversed {
                (right, left)
            } else {
                (left, right)
            };
            let mut udp = [0; 12];
            packet_wire::udp::emit(&mut udp, source, destination, ports.0, ports.1).unwrap();
            if corrupt {
                udp[8] ^= 1;
            }
            inject(nic, ip_frame(source, destination, 17, &udp));
        }
    };
    let echo_pair = |nic, reversed, reply, identifier: u16| {
        for (left, right) in peers {
            let (source, destination) = if reversed {
                (right, left)
            } else {
                (left, right)
            };
            let mut icmp = [0; 1350];
            icmp[4..6].copy_from_slice(&identifier.to_be_bytes());
            let protocol = match (source, destination) {
                (IpAddr::V4(_), IpAddr::V4(_)) => {
                    packet_wire::icmpv4::emit(&mut icmp, if reply { 0 } else { 8 }, 0).unwrap();
                    1
                }
                (IpAddr::V6(src), IpAddr::V6(dst)) => {
                    packet_wire::icmpv6::emit(
                        &mut icmp,
                        if reply { 129 } else { 128 },
                        0,
                        src,
                        dst,
                    )
                    .unwrap();
                    58
                }
                _ => unreachable!(),
            };
            inject(nic, ip_frame(source, destination, protocol, &icmp));
        }
    };
    let error_pair = |originals: &[Vec<u8>], packet_too_big, corrupt| {
        for frame in originals {
            match frame[12..14] {
                [0x08, 0x00] => {
                    let packet = Ipv4Packet::parse(&frame[14..]).unwrap();
                    let mut error = [0; 36];
                    error[8..].copy_from_slice(&packet.bytes()[..28]);
                    if packet_too_big {
                        error[6..8].copy_from_slice(&1280u16.to_be_bytes());
                    }
                    packet_wire::icmpv4::emit(&mut error, 3, if packet_too_big { 4 } else { 3 })
                        .unwrap();
                    if corrupt {
                        error[2] ^= 1;
                    }
                    inject(
                        &b,
                        ip_frame(
                            packet.destination().into(),
                            packet.source().into(),
                            1,
                            &error,
                        ),
                    );
                }
                [0x86, 0xdd] => {
                    let packet = ip(frame);
                    let mut error = [0; 56];
                    error[8..].copy_from_slice(&packet.bytes()[..48]);
                    if packet_too_big {
                        error[4..8].copy_from_slice(&1280u32.to_be_bytes());
                    }
                    packet_wire::icmpv6::emit(
                        &mut error,
                        if packet_too_big { 2 } else { 1 },
                        if packet_too_big { 0 } else { 4 },
                        packet.destination(),
                        packet.source(),
                    )
                    .unwrap();
                    if corrupt {
                        error[2] ^= 1;
                    }
                    inject(
                        &b,
                        ip_frame(
                            packet.destination().into(),
                            packet.source().into(),
                            58,
                            &error,
                        ),
                    );
                }
                _ => {}
            }
        }
    };
    let is_icmp = |frame: &Vec<u8>| match frame[12..14] {
        [0x08, 0x00] => Ipv4Packet::parse(&frame[14..]).unwrap().protocol() == 1,
        [0x86, 0xdd] => {
            let packet = ip(frame);
            packet.upper_protocol() == 58
                && matches!(packet.upper_layer().unwrap()[0], 1..=4 | 128 | 129)
        }
        _ => false,
    };
    let forwarded = |nic| {
        frames(nic)
            .into_iter()
            .filter(|frame| match frame[12..14] {
                [0x08, 0x00] => Ipv4Packet::parse(&frame[14..]).unwrap().protocol() == 17,
                [0x86, 0xdd] => ip(frame).upper_protocol() == 17,
                _ => false,
            })
            .count()
    };

    // 校验失败的首包与仍在等待出口的首包均未实际提交，不能授权回包。
    inject_pair(&a, false, (41200, 53), true);
    poll(&net, 2003);
    assert_eq!(forwarded(&b), 0);
    b.transmit_available.store(false, Relaxed);
    inject_pair(&a, false, (41200, 53), false);
    poll(&net, 2004);
    assert_eq!(forwarded(&b), 0);
    inject_pair(&b, true, (53, 41200), false);
    poll(&net, 2005);
    assert_eq!(forwarded(&a), 0);
    assert!(connections(first_page, 2005).rows.is_empty());
    let checkpoint = changes(ChangeCursor::default());
    assert!(checkpoint.reset && checkpoint.changes.is_empty());
    let wait = |cursor| crate::inet::stack(&net).unwrap().lock().unwrap()
        .arm_connection_watch(cursor, || Ok(Arc::new(crate::Event::default()))).unwrap();
    let event = wait(checkpoint.cursor).unwrap();
    assert!(Arc::ptr_eq(&event, &wait(checkpoint.cursor).unwrap()));
    b.transmit_available.store(true, Relaxed);
    b.transmit_ready.store(true, Relaxed);
    poll(&net, 2006);
    let udp_originals = frames(&b);
    assert_eq!(udp_originals.len(), 2);
    let active = stack(&net).unwrap().receive_snapshot().unwrap();
    a.wire.lock().unwrap().2.receive_snapshot = Some((2, active.clone()));
    b.wire.lock().unwrap().2.receive_snapshot = Some((3, active));
    {
        inject_pair(&a, false, (41200, 53), false);
        let network = stack(&net).unwrap().lock().unwrap();
        let mut slots = [const { None }; 2];
        let batch = network.interfaces[1].device.receive_batch(&mut slots, true).unwrap();
        for (rx, _reply) in batch {
            assert_ne!(rx.receive_flow(net.id()).unwrap().observation[0], 0);
            assert!(rx.receive_flow(net.id() + 1).is_none());
        }
    }
    let opening = connections(first_page, 2006);
    let opened = changes(checkpoint.cursor);
    assert_eq!(opened.changes, opening.rows.iter().copied().map(Change::Added).collect::<Vec<_>>());
    assert_ne!(event.0.load(Relaxed) & crate::native_abi::signals::USER_0, 0);
    assert!(wait(checkpoint.cursor).is_none());
    assert_eq!(opening.rows.len(), 2);
    for row in &opening.rows {
        assert!(peers.contains(&(row.source.ip(), row.destination.ip())));
        assert_eq!((row.source.port(), row.destination.port()), (41200, 53));
        assert_eq!((row.protocol, row.phase, row.idle_ms), (Protocol::Udp, Phase::Opening, 30_000));
    }
    error_pair(&udp_originals, false, true);
    poll(&net, 2007);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 0);
    error_pair(&udp_originals, false, false);
    poll(&net, 2007);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 2);

    inject_pair(&b, true, (53, 41201), false);
    inject_pair(&b, true, (53, 41200), true);
    poll(&net, 2007);
    assert_eq!(forwarded(&a), 0);
    let udp_fragments = |source, destination, ports: (u16, u16), corrupt| {
        let mut udp = [0; 24];
        udp[8..].copy_from_slice(b"fragment payload");
        packet_wire::udp::emit(&mut udp, source, destination, ports.0, ports.1).unwrap();
        if corrupt {
            udp[23] ^= 1;
        }
        [(0, true, &udp[..16]), (16, false, &udp[16..])].map(|(offset, more, payload)| {
            match (source, destination) {
                (IpAddr::V4(source), IpAddr::V4(destination)) => {
                    let flags = (offset as u16 / 8) | (u16::from(more) << 13);
                    let mut bytes = ipv4_forwarding::datagram(64, flags, &[], payload);
                    Ipv4Header {
                        source,
                        destination,
                        protocol: 17,
                        hop_limit: 64,
                        dscp_ecn: 0,
                        identification: 300,
                        fragment: flags,
                        options: &[],
                    }
                    .emit(&mut bytes[14..])
                    .unwrap();
                    bytes
                }
                (IpAddr::V6(source), IpAddr::V6(destination)) => {
                    let mut fragment = [0; 24];
                    packet_wire::ipv6::Fragment {
                        next_header: 17,
                        offset,
                        more,
                        identification: 300,
                    }
                    .emit(&mut fragment[..8])
                    .unwrap();
                    fragment[8..8 + payload.len()].copy_from_slice(payload);
                    frame(source, destination, 64, 44, &fragment[..8 + payload.len()])
                }
                _ => unreachable!(),
            }
        })
    };
    // 回包乱序到达。完整校验前不能发出任意一片或推进连接，排队时仍等到实际提交。
    for corrupt in [true, false] {
        let fragments = peers
            .map(|(destination, source)| udp_fragments(source, destination, (53, 41200), corrupt));
        for [_, last] in &fragments {
            inject(&b, last.clone());
        }
        poll(&net, 2007);
        assert!(frames(&a).is_empty());
        assert!(changes(opened.cursor).changes.is_empty());
        if !corrupt {
            a.transmit_available.store(false, Relaxed);
        }
        for [first, _] in &fragments {
            inject(&b, first.clone());
        }
        poll(&net, 2007);
        assert!(frames(&a).is_empty());
        assert!(changes(opened.cursor).changes.is_empty());
    }
    a.transmit_available.store(true, Relaxed);
    a.transmit_ready.store(true, Relaxed);
    poll(&net, 2008);
    let reply_fragments = frames(&a);
    assert_eq!(reply_fragments.len(), 4);
    let pieces = |ether_type| {
        reply_fragments
            .iter()
            .filter(move |frame| frame[12..14] == ether_type)
    };
    assert_eq!(
        pieces([0x08, 0x00])
            .map(|frame| {
                let ip = Ipv4Packet::parse(&frame[14..]).unwrap();
                (
                    ip.identification(),
                    ip.hop_limit(),
                    ip.fragment_offset(),
                    ip.more_fragments(),
                )
            })
            .collect::<Vec<_>>(),
        [(300, 63, 0, true), (300, 63, 16, false)]
    );
    assert_eq!(
        pieces([0x86, 0xdd])
            .map(|frame| {
                let ip = ip(frame);
                let piece = ip.extensions().next().unwrap().fragment().unwrap();
                (
                    piece.identification(),
                    ip.hop_limit(),
                    piece.fragment_offset(),
                    piece.more(),
                )
            })
            .collect::<Vec<_>>(),
        [(300, 63, 0, true), (300, 63, 16, false)]
    );
    let cursor = Cursor {
        epoch: opening.epoch,
        after: opening.rows[0].id,
    };
    let established = connections(cursor, 2008);
    let replied = changes(opened.cursor);
    assert_eq!(replied.changes.len(), 2);
    assert!(replied.changes.iter().all(|change| matches!(change, Change::Updated(row) if row.phase == Phase::Established)));
    assert_eq!(established.rows.len(), 1);
    assert_eq!(established.rows[0].id, opening.rows[1].id);
    assert_eq!((established.rows[0].phase, established.rows[0].idle_ms), (Phase::Established, 120_000));

    echo_pair(&a, false, true, 61);
    echo_pair(&b, true, true, 61);
    poll(&net, 2009);
    assert!(frames(&a).is_empty() && frames(&b).is_empty());
    echo_pair(&a, false, false, 61);
    poll(&net, 2009);
    let echo_originals: Vec<_> = frames(&b).into_iter().filter(is_icmp).collect();
    assert_eq!(echo_originals.len(), 2);
    error_pair(&echo_originals, true, false);
    poll(&net, 2009);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 2);

    // 反向同 ID 的请求仍为另一条新流，不能借前向流或关联错误获得放行。
    echo_pair(&b, true, false, 61);
    echo_pair(&a, false, true, 61);
    echo_pair(&b, true, true, 62);
    poll(&net, 2009);
    assert!(frames(&a).is_empty() && frames(&b).is_empty());
    echo_pair(&b, true, true, 61);
    poll(&net, 2009);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 2);

    inject_pair(&a, false, (41201, 53), false);
    poll(&net, 2009);
    assert_eq!(forwarded(&b), 2);
    // 关联错误即使在即将过期时成功转发，也不能延长 Echo 的寿命。
    error_pair(&echo_originals, true, false);
    poll(&net, 31_999);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 2);
    inject_pair(&b, true, (53, 41201), false);
    inject_pair(&b, true, (53, 41200), false);
    poll(&net, 32_010);
    assert_eq!(
        forwarded(&a),
        2,
        "only the previously replied flow may pass"
    );
    echo_pair(&b, true, true, 61);
    error_pair(&echo_originals, true, false);
    poll(&net, 32_011);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 0);

    let tcp_frame = |source, destination, header: TcpHeader, data: &[u8]| {
        let mut bytes = vec![0; 24 + data.len()];
        if header.flags & tcp::SYN != 0 {
            bytes[20..24].copy_from_slice(&[3, 3, 0, 0]);
        }
        bytes[24..].copy_from_slice(data);
        header.emit(&mut bytes, 24, source, destination).unwrap();
        ip_frame(source, destination, 6, &bytes)
    };
    let tcp_pair = |nic, reversed, flags, sequence, acknowledgment, data: &[u8]| {
        for (left, right) in peers {
            let (source, destination, source_port, destination_port) = if reversed {
                (right, left, 443, 49100)
            } else {
                (left, right, 49100, 443)
            };
            let header = TcpHeader {
                source_port,
                destination_port,
                sequence,
                acknowledgment,
                flags,
                window: 4096,
                urgent_pointer: 0,
            };
            inject(nic, tcp_frame(source, destination, header, data));
        }
    };
    let is_tcp = |frame: &Vec<u8>| match frame[12..14] {
        [0x08, 0x00] => Ipv4Packet::parse(&frame[14..]).unwrap().protocol() == 6,
        [0x86, 0xdd] => ip(frame).upper_protocol() == 6,
        _ => false,
    };
    let tcp_count = |nic| frames(nic).into_iter().filter(is_tcp).count();
    let syn = u32::MAX - 1;
    let initial_ack = syn + 1;
    tcp_pair(&a, false, tcp::ACK, initial_ack, 301, b"");
    tcp_pair(&b, true, tcp::SYN | tcp::ACK, 300, initial_ack, b"");
    poll(&net, 32_012);
    assert_eq!((tcp_count(&a), tcp_count(&b)), (0, 0));
    for now in [32_013, 32_014] {
        tcp_pair(&a, false, tcp::SYN, syn, 0, b"");
        poll(&net, now);
        assert_eq!(tcp_count(&b), 2);
    }
    tcp_pair(
        &b,
        true,
        tcp::SYN | tcp::ACK,
        300,
        initial_ack.wrapping_add(1),
        b"",
    );
    poll(&net, 32_015);
    assert_eq!(tcp_count(&a), 0);
    tcp_pair(&b, true, tcp::SYN | tcp::ACK, 300, initial_ack, b"");
    poll(&net, 32_016);
    assert_eq!(tcp_count(&a), 2);
    tcp_pair(&a, false, tcp::ACK, initial_ack, 301, b"");
    poll(&net, 32_017);
    assert_eq!(tcp_count(&b), 2);
    tcp_pair(&a, false, tcp::ACK, initial_ack, 301, b"wrap");
    poll(&net, 32_018);
    let data: Vec<_> = frames(&b).into_iter().filter(is_tcp).collect();
    assert_eq!(data.len(), 2);
    let mut outside_window = data.clone();
    for frame in &mut outside_window {
        let transport = if frame[12..14] == [0x08, 0x00] {
            34
        } else {
            54
        };
        frame[transport + 4..transport + 8].copy_from_slice(&0x4000_0000u32.to_be_bytes());
    }
    error_pair(&outside_window, false, false);
    poll(&net, 32_019);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 0);
    error_pair(&data, false, false);
    poll(&net, 32_019);
    assert_eq!(frames(&a).into_iter().filter(is_icmp).count(), 2);

    tcp_pair(&b, true, tcp::ACK, 301, 4, b"");
    tcp_pair(&b, true, tcp::RST | tcp::ACK, 10_000, 3, b"");
    poll(&net, 32_020);
    assert_eq!(tcp_count(&a), 0);
    tcp_pair(&b, true, tcp::ACK, 301, 3, b"");
    poll(&net, 32_021);
    assert_eq!(tcp_count(&a), 2);

    tcp_pair(&a, false, tcp::FIN | tcp::ACK, 3, 301, b"");
    poll(&net, 32_022);
    assert_eq!(tcp_count(&b), 2);
    tcp_pair(&b, true, tcp::ACK, 301, 4, b"fin");
    poll(&net, 32_023);
    assert_eq!(tcp_count(&a), 2);
    let closing = connections(first_page, 32_023);
    let tcp_ids: Vec<_> = closing.rows.iter().filter(|row| row.protocol == Protocol::Tcp).map(|row| {
        assert_eq!(row.phase, Phase::Closing);
        row.id
    }).collect();
    assert_eq!(tcp_ids.len(), 2);
    tcp_pair(&b, true, tcp::RST | tcp::ACK, 304, 4, b"");
    poll(&net, 32_024);
    assert_eq!(tcp_count(&a), 2);
    tcp_pair(&a, false, tcp::ACK, 4, 304, b"");
    poll(&net, 32_024);
    assert_eq!(tcp_count(&b), 0);
    tcp_pair(&a, false, tcp::SYN, 51, 0, b"");
    poll(&net, 32_024);
    assert_eq!(tcp_count(&b), 2);
    let reopened = connections(first_page, 32_024);
    let tcp_rows: Vec<_> = reopened.rows.iter().filter(|row| row.protocol == Protocol::Tcp).collect();
    assert_eq!(tcp_rows.len(), 2);
    assert!(tcp_rows.iter().all(|row| row.phase == Phase::Opening && row.id > tcp_ids[1]));

    inject_pair(&b, true, (53, 41200), false);
    poll(&net, 152_011);
    assert_eq!(forwarded(&a), 0, "idle expiry closes the reverse direction");
    assert!(connections(first_page, 152_024).rows.is_empty());

    // 跨页读取当前有效连接；原地更新与到期清理不能混入上一代的 ID。
    for port in 0..=PAGE_LIMIT as u16 / 2 {
        inject_pair(&a, false, (42000 + port, 53), false);
        poll(&net, 152_025);
        assert_eq!(forwarded(&b), 2);
    }
    let first = connections(first_page, 152_025);
    assert_eq!(first.rows.len(), PAGE_LIMIT);
    assert_eq!(first.next, first.rows.last().map(|row| row.id));
    let cursor = Cursor { epoch: first.epoch, after: first.next.unwrap() };
    let rest = connections(cursor, 152_025);
    assert_eq!(rest.rows.len(), 2);
    assert_eq!(rest.next, None);
    assert!(rest.rows.iter().all(|row| {
        row.id > cursor.after && row.source.port() == 42000 + PAGE_LIMIT as u16 / 2
    }));
    let stale = Cursor { epoch: first.epoch.wrapping_add(1).max(1), ..cursor };
    assert_eq!(
        crate::inet::stack(&net).unwrap().lock().unwrap()
            .connections_at(stale, Instant::from_millis(152_025)),
        Err(crate::native_abi::ERR_BAD_STATE),
    );

    // 只清除一个地址族的流。该流已获放行但仍在等待 TX 的回包和 ICMP 错误必须取消。
    let target = first.rows.iter().find(|row| row.source.is_ipv4() && row.source.port() == 42000).unwrap();
    let identity = Identity { epoch: first.epoch, id: target.id };
    let clear = |identity: Identity, now| {
        let identity = Identity::parse(&identity.encode()).unwrap();
        crate::inet::stack(&net).unwrap().lock().unwrap()
            .clear_connection_at(identity, Instant::from_millis(now))
    };
    assert!(Identity::parse(&first_page.encode()).is_err());
    inject_pair(&a, false, (42000, 53), false);
    poll(&net, 152_026);
    let originals = frames(&b);
    assert_eq!(originals.len(), 2);
    a.transmit_available.store(false, Relaxed);
    inject_pair(&b, true, (53, 42000), false);
    error_pair(&originals, false, false);
    poll(&net, 152_027);
    assert!(frames(&a).is_empty());
    assert_eq!(
        clear(Identity { epoch: stale.epoch, ..identity }, 152_027),
        Err(crate::native_abi::ERR_NOT_FOUND),
    );
    let before_clear = changes(ChangeCursor::default()).cursor;
    clear(identity, 152_027).unwrap();
    assert_eq!(changes(before_clear).changes, [Change::Removed(identity.id)]);
    assert!(connections(first_page, 152_027).rows.iter().all(|row| row.id != identity.id));
    a.transmit_available.store(true, Relaxed);
    a.transmit_ready.store(true, Relaxed);
    poll(&net, 152_028);
    let transmitted = frames(&a);
    assert_eq!(transmitted.len(), 2);
    assert!(transmitted.iter().all(|frame| frame[12..14] == [0x86, 0xdd]));
    assert_eq!(transmitted.iter().filter(|frame| is_icmp(frame)).count(), 1);

    inject_pair(&b, true, (53, 42000), false);
    poll(&net, 152_029);
    assert_eq!(forwarded(&a), 1);
    inject_pair(&a, false, (42000, 53), false);
    poll(&net, 152_030);
    assert_eq!(forwarded(&b), 2);
    let replacement_cursor = Cursor { epoch: first.epoch, after: rest.rows.last().unwrap().id };
    let replacement = connections(replacement_cursor, 152_030);
    assert_eq!(replacement.rows.len(), 1);
    assert_eq!(replacement.rows[0].source, target.source);
    assert_eq!(clear(identity, 152_030), Err(crate::native_abi::ERR_NOT_FOUND));
    assert_eq!(connections(replacement_cursor, 152_030), replacement);
    for port in 43000..43513 {
        inject_pair(&a, false, (port, 53), false);
        poll(&net, 152_031);
        assert_eq!(forwarded(&b), 2);
    }
    assert!(changes(before_clear).reset);

    // 转换规则从同一配置事务发布。目标在转发域内，先改写再选路。
    let install = |config: &[u8]| {
        let config = router_protocol::configuration::Configuration::decode(config).unwrap();
        let body = config.firewall_source(|_| Ok(None)).unwrap();
        let batch = network_protocol::ConfigurationBatch::default()
            .with_prerouting(router_protocol::firewall::Prerouting::parse(&body).unwrap());
        let tx = crate::inet::prepare_configuration(&net, Some(batch), None).unwrap();
        tx.activate().unwrap();
        tx.confirm().unwrap();
    };
    install(br#"{"version":1,"firewall":{"prerouting":[
        {"destination":"10.41.0.200","prefix_length":30,"transport":{"protocol":"udp","destination_port":53},
         "action":{"kind":"destination_nat","address":"10.41.0.9","port":5300}},
        {"destination":"2001:db8:29::c8","prefix_length":126,"transport":{"protocol":"udp","destination_port":53},
         "action":{"kind":"destination_nat","address":"2001:db8:29::9","port":5300}},
        {"destination":"10.41.0.200","prefix_length":30,"transport":{"protocol":"tcp","destination_port":53},
         "action":{"kind":"destination_nat","address":"10.41.0.9","port":8053}},
        {"destination":"2001:db8:29::c8","prefix_length":126,"transport":{"protocol":"tcp","destination_port":53},
         "action":{"kind":"destination_nat","address":"2001:db8:29::9","port":8053}},
        {"connection_state":"established","action":{"kind":"accept"}},
        {"connection_state":"related","action":{"kind":"accept"}},
        {"action":{"kind":"drop"}}
    ]}}"#);
    let checkpoint = changes(ChangeCursor::default());
    let public =
        ["10.41.0.201", "2001:db8:29::c9"].map(|address| address.parse::<IpAddr>().unwrap());
    let request = |public: [IpAddr; 2], port| {
        for ((source, _), destination) in peers.into_iter().zip(public) {
            let mut udp = [0; 12];
            packet_wire::udp::emit(&mut udp, source, destination, 49_000, port).unwrap();
            inject(&a, ip_frame(source, destination, 17, &udp));
        }
    };
    b.transmit_available.store(false, Relaxed);
    request(public, 53);
    poll(&net, 152_032);
    assert!(frames(&b).is_empty());
    inject_pair(&b, true, (5300, 49_000), false);
    poll(&net, 152_033);
    assert!(frames(&a).is_empty() && changes(checkpoint.cursor).changes.is_empty());
    b.transmit_available.store(true, Relaxed);
    b.transmit_ready.store(true, Relaxed);
    poll(&net, 152_034);
    let requests = frames(&b);
    assert_eq!(requests.len(), 2);
    for (frame, (source, destination)) in requests.iter().zip(peers) {
        let (wire_source, wire_destination, payload) = wire_payload(frame);
        assert_eq!((wire_source, wire_destination), (source, destination));
        let udp = packet_wire::udp::UdpPacket::parse(payload, source, destination).unwrap();
        assert_eq!((udp.source_port(), udp.destination_port()), (49_000, 5300));
    }
    let added = changes(checkpoint.cursor);
    assert_eq!(added.changes.len(), 2);
    let collisions = ["10.41.0.202", "2001:db8:29::ca"].map(|address| address.parse().unwrap());
    request(collisions, 53);
    request(public, 54);
    poll(&net, 152_035);
    assert!(frames(&b).is_empty() && changes(added.cursor).changes.is_empty());

    error_pair(&requests, true, false);
    poll(&net, 152_036);
    let errors = frames(&a);
    assert_eq!(errors.len(), 2);
    let packet = Ipv4Packet::parse(&errors[0][14..]).unwrap();
    assert_eq!(IpAddr::from(packet.source()), public[0]);
    let error = packet_wire::icmpv4::Icmpv4Packet::parse(packet.payload()).unwrap();
    assert_eq!(error.message_type(), 3);
    let quote = packet_wire::ipv4::Ipv4Quote::parse(&packet.payload()[8..]).unwrap();
    assert_eq!(IpAddr::from(quote.destination()), public[0]);
    let mut expected_udp = [0; 12];
    packet_wire::udp::emit(&mut expected_udp, peers[0].0, public[0], 49_000, 53).unwrap();
    assert_eq!(quote.upper_layer().unwrap(), &expected_udp[..8]);
    let packet = ip(&errors[1]);
    assert_eq!(IpAddr::from(packet.source()), public[1]);
    let error = packet_wire::icmpv6::Icmpv6Packet::parse(
        packet.upper_layer().unwrap(),
        packet.source(),
        packet.destination(),
    )
    .unwrap();
    assert_eq!(error.message_type(), 2);
    let quote = packet_wire::ipv6::Ipv6Quote::parse(&error.bytes()[8..]).unwrap();
    assert_eq!(IpAddr::from(quote.destination()), public[1]);
    packet_wire::udp::emit(&mut expected_udp, peers[1].0, public[1], 49_000, 53).unwrap();
    assert_eq!(quote.upper_layer().unwrap(), &expected_udp[..8]);

    // 相同虚拟地址与端口按 TCP/UDP 分流。TCP 握手的回复也恢复原端口。
    let mut tcp_cursor = None;
    for reverse in [false, true] {
        for (family, (client, server)) in peers.into_iter().enumerate() {
            let (source, destination, ports, flags) = if reverse {
                (server, client, (8053, 49_000), tcp::SYN | tcp::ACK)
            } else {
                (client, public[family], (49_000, 53), tcp::SYN)
            };
            let header = TcpHeader {
                source_port: ports.0,
                destination_port: ports.1,
                sequence: if reverse { 200 } else { 100 },
                acknowledgment: if reverse { 101 } else { 0 },
                flags,
                window: 4096,
                urgent_pointer: 0,
            };
            inject(
                if reverse { &b } else { &a },
                tcp_frame(source, destination, header, b""),
            );
        }
        poll(&net, 152_036);
        let output = frames(if reverse { &a } else { &b });
        assert_eq!(output.len(), 2);
        for (family, frame) in output.iter().enumerate() {
            let (source, destination, bytes) = wire_payload(frame);
            let (client, server) = peers[family];
            assert_eq!(
                (source, destination),
                if reverse {
                    (public[family], client)
                } else {
                    (client, server)
                }
            );
            let header = tcp::TcpPacket::parse(bytes, source, destination)
                .unwrap()
                .header();
            assert_eq!(
                (header.source_port(), header.destination_port()),
                if reverse {
                    (53, 49_000)
                } else {
                    (49_000, 8053)
                }
            );
        }
        if !reverse {
            tcp_cursor = Some(changes(added.cursor));
        }
    }
    for change in tcp_cursor.unwrap().changes {
        let Change::Added(row) = change else {
            panic!("TCP SYN starts a new flow")
        };
        clear(
            Identity {
                epoch: checkpoint.cursor.epoch,
                id: row.id,
            },
            152_036,
        )
        .unwrap();
    }

    install(br#"{"version":1}"#);
    for (destination, source) in peers {
        let [first, last] = udp_fragments(source, destination, (5300, 49_000), false);
        inject(&b, last);
        inject(&b, first);
    }
    poll(&net, 152_037);
    let replies = frames(&a);
    assert_eq!(replies.len(), 4);
    for (family, pieces) in replies.as_chunks::<2>().0.iter().enumerate() {
        assert_fragmented_udp(pieces, (public[family], peers[family].0), (53, 49_000));
    }
    for change in added.changes {
        let Change::Added(row) = change else {
            panic!("NAT starts a new flow");
        };
        clear(
            Identity {
                epoch: checkpoint.cursor.epoch,
                id: row.id,
            },
            152_037,
        )
        .unwrap();
    }
    inject_pair(&b, true, (5300, 49_000), false);
    poll(&net, 152_038);
    let replies = frames(&a);
    assert_eq!(replies.len(), 2);
    assert_eq!(
        IpAddr::from(Ipv4Packet::parse(&replies[0][14..]).unwrap().source()),
        peers[0].1
    );
    assert_eq!(IpAddr::from(ip(&replies[1]).source()), peers[1].1);

    install(
        br#"{"version":1,"firewall":{"prerouting":[
        {"transport":{"protocol":"udp","destination_port":53},"action":{"kind":"accept"}},
        {"transport":{"protocol":"tcp","destination_port":443},"action":{"kind":"accept"}},
        {"action":{"kind":"drop"}}
    ]}}"#,
    );
    let checkpoint = changes(ChangeCursor::default());
    let port_policy = stack(&net).unwrap().receive_snapshot().unwrap();
    inject_pair(&a, false, (49_100, 54), false);
    inject_pair(&a, false, (49_100, 53), false);
    inject_pair(&a, false, (49_100, 53), true);
    tcp_pair(&a, false, tcp::SYN, 600, 0, b"");
    {
        use crate::inet::protocol_owner::receive_mark;
        use packet_wire::batch::NetworkPacket;
        let mut wire = a.wire.lock().unwrap();
        assert_eq!(wire.0.len(), 8);
        for (frame, expected) in wire.0.iter().zip([None, None].into_iter().chain([Some(0); 6])) {
            let packet = NetworkPacket::direct_ip(frame).unwrap();
            let prepared = port_policy.prepare(packet, true, 2);
            assert!(prepared.verify_transport);
            assert_eq!(receive_mark(&prepared.annotation), expected);
        }
        wire.2.receive_snapshot = Some((2, port_policy));
    }
    poll(&net, 152_039);
    let transmitted = frames(&b);
    assert_eq!(transmitted.len(), 4);
    assert_eq!(transmitted.iter().filter(|frame| is_tcp(frame)).count(), 2);
    assert!(changes(checkpoint.cursor).changes.is_empty());
    let counters = stack(&net)
        .unwrap()
        .lock()
        .unwrap()
        .firewall
        .counter_page(router_protocol::firewall::counters::Cursor::default())
        .unwrap();
    assert_eq!(
        counters.rows.iter().map(|row| row.packets).collect::<Vec<_>>(),
        [2, 2, 4],
    );
    install(br#"{"version":1,"firewall":{"prerouting":[{"action":{"kind":"drop"}}]}}"#);
    inject_pair(&a, false, (49_100, 53), false);
    poll(&net, 152_040);
    assert_eq!(forwarded(&b), 0);

    // 接口地址上的两个本机服务，只有命中 DNAT 的流量交给后端。
    let local = [IpAddr::from(Ipv4Addr::new(10, 40, 0, 15)), A.into()];
    let receivers = local.map(|address| {
        [5301, 5303].map(|port| {
            let family = if address.is_ipv4() {
                network_protocol::InetFamily::Ipv4
            } else {
                network_protocol::InetFamily::Ipv6
            };
            let socket = InetSocket::new(
                family,
                InetProtocol::Udp,
                Arc::new(crate::Event::default()),
                net.clone(),
            )
            .unwrap();
            socket.bind(InetAddress::new(address, port)).unwrap();
            socket
        })
    });
    install(br#"{"version":1,"firewall":{"prerouting":[
        {"destination":"10.40.0.15","prefix_length":32,"transport":{"protocol":"udp","destination_port":5301},
         "action":{"kind":"destination_nat","address":"10.41.0.9","port":5302}},
        {"destination":"2001:db8:28::f","prefix_length":128,"transport":{"protocol":"udp","destination_port":5301},
         "action":{"kind":"destination_nat","address":"2001:db8:29::9","port":5302}},
        {"action":{"kind":"drop"}}
    ]}}"#);
    for (port, now) in [(5301, 152_040), (5303, 152_041)] {
        for ((source, _), destination) in peers.into_iter().zip(local) {
            let [first, last] = udp_fragments(source, destination, (49_001, port), false);
            inject(&a, last);
            inject(&a, first);
        }
        poll(&net, now);
        assert_eq!(forwarded(&b), if port == 5301 { 4 } else { 0 });
        for [forwarded, host] in &receivers {
            assert!(!forwarded.poll_state().readable);
            assert_eq!(host.poll_state().readable, port == 5303);
        }
    }
    install(br#"{"version":1}"#);
    for ((source, _), destination) in peers.into_iter().zip(local) {
        let mut udp = [0; 12];
        packet_wire::udp::emit(&mut udp, source, destination, 49_001, 5301).unwrap();
        let mut frame = ip_frame(source, destination, 17, &udp);
        if source.is_ipv4() {
            frame.resize(60, 0);
        }
        inject(&a, frame);
    }
    poll(&net, 152_042);
    let requests = frames(&b);
    assert_eq!(requests.len(), 2);
    let v4 = Ipv4Packet::parse(&requests[0][14..]).unwrap();
    let udp = packet_wire::udp::UdpPacket::parse(v4.payload(), peers[0].0, peers[0].1).unwrap();
    assert_eq!(udp.destination_port(), 5302);
    assert!(
        receivers
            .iter()
            .all(|[forwarded, _]| !forwarded.poll_state().readable)
    );
    inject_pair(&b, true, (5302, 49_001), false);
    poll(&net, 152_043);
    let replies = frames(&a);
    assert_eq!(replies.len(), 2);
    for (family, frame) in replies.iter().enumerate() {
        let (source, destination, bytes) = wire_payload(frame);
        assert_eq!((source, destination), (local[family], peers[family].0));
        let udp = packet_wire::udp::UdpPacket::parse(bytes, source, destination).unwrap();
        assert_eq!(udp.source_port(), 5301);
    }
    for socket in receivers.into_iter().flatten() {
        socket.close_description();
    }

    // 固定源转换使用出口接口地址。回包、分片及相关错误仍须从本机目的送回内网。
    install(
        br#"{"version":1,"firewall":{"prerouting":[
        {"source":{"network":"10.40.0.0","length":24},"transport":{"protocol":"udp"},
         "action":{"kind":"source_nat","address":"10.41.0.15","port":5300}},
        {"source":{"network":"2001:db8:28::","length":64},
         "action":{"kind":"source_nat","address":"2001:db8:29::f"}},
        {"connection_state":"established","action":{"kind":"accept"}},
        {"connection_state":"related","action":{"kind":"accept"}},
        {"action":{"kind":"drop"}}
    ]}}"#,
    );
    let outside = [IpAddr::from(Ipv4Addr::new(10, 41, 0, 15)), B.into()];
    let ports = [5300, 49_002];
    inject_pair(&a, false, (49_002, 53), false);
    poll(&net, 152_044);
    let requests = frames(&b);
    assert_eq!(requests.len(), 2);
    for (family, frame) in requests.iter().enumerate() {
        let (source, destination, bytes) = wire_payload(frame);
        assert_eq!((source, destination), (outside[family], peers[family].1));
        let udp = packet_wire::udp::UdpPacket::parse(bytes, source, destination).unwrap();
        assert_eq!(
            (udp.source_port(), udp.destination_port()),
            (ports[family], 53)
        );
    }
    error_pair(&requests, true, false);
    poll(&net, 152_045);
    let errors = frames(&a);
    assert_eq!(errors.len(), 2);
    for (family, error) in errors.iter().enumerate() {
        let (source, destination, payload) = wire_payload(error);
        let (client, server) = peers[family];
        assert_eq!((source, destination), (server, client));
        let mut udp = [0; 12];
        packet_wire::udp::emit(&mut udp, client, server, 49_002, 53).unwrap();
        let (quoted_source, quoted_payload) = if family == 0 {
            let quote = packet_wire::ipv4::Ipv4Quote::parse(&payload[8..]).unwrap();
            (IpAddr::from(quote.source()), quote.upper_layer().unwrap())
        } else {
            let quote = packet_wire::ipv6::Ipv6Quote::parse(&payload[8..]).unwrap();
            (IpAddr::from(quote.source()), quote.upper_layer().unwrap())
        };
        assert_eq!((quoted_source, quoted_payload), (client, &udp[..8]));
        let checksum = if family == 0 {
            packet_wire::checksum::checksum(payload)
        } else {
            packet_wire::checksum::transport_checksum(source, destination, 58, payload).unwrap()
        };
        assert_eq!(checksum, 0);
    }
    for (family, (_, server)) in peers.into_iter().enumerate() {
        let [first, last] = udp_fragments(server, outside[family], (53, ports[family]), false);
        inject(&b, last);
        inject(&b, first);
    }
    poll(&net, 152_046);
    let replies = frames(&a);
    assert_eq!(replies.len(), 4);
    for (family, pieces) in replies.as_chunks::<2>().0.iter().enumerate() {
        let (client, server) = peers[family];
        assert_fragmented_udp(pieces, (server, client), (53, 49_002));
    }
}
