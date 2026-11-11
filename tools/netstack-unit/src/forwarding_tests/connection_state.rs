use super::super::ipv4_forwarding;
use super::*;

#[test]
fn stateful_rules_follow_dual_stack_transmit_and_idle_expiry() {
    use core::{net::IpAddr, sync::atomic::Ordering::Relaxed};
    use packet_wire::{
        ipv4::{Ipv4Header, Ipv4Packet},
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
    inject_pair(&b, true, (53, 41200), false);
    poll(&net, 2008);
    assert_eq!(forwarded(&a), 2);
    let cursor = Cursor { epoch: opening.epoch, after: opening.rows[0].id };
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

    let tcp_pair = |nic, reversed, flags, sequence, acknowledgment, data: &[u8]| {
        for (left, right) in peers {
            let (source, destination, source_port, destination_port) = if reversed {
                (right, left, 443, 49100)
            } else {
                (left, right, 49100, 443)
            };
            let mut bytes = vec![0; 20 + data.len()];
            bytes[20..].copy_from_slice(data);
            TcpHeader {
                source_port,
                destination_port,
                sequence,
                acknowledgment,
                flags,
                window: 4096,
                urgent_pointer: 0,
            }
            .emit(&mut bytes, 20, source, destination)
            .unwrap();
            inject(nic, ip_frame(source, destination, 6, &bytes));
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
}
