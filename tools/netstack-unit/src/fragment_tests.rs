// 重组的乱序/重复、长度、checksum、寿命与控制协议安全跨越 RX loan/IP/transport，静态审查不足。
// 沿已有生产 NET/NIC fixture 观察实际报文及 socket 结果，不复制重组算法或添加生产测试入口。
mod fragment_contract {
    use super::*;
    use core::net::{IpAddr, Ipv6Addr};
    use packet_wire::{
        ethernet::{EthernetFrame, EthernetHeader},
        ipv4::{Ipv4Header, Ipv4Packet},
        ipv6::{Ipv6Header, Ipv6Packet},
    };
    const LOCAL4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const PEER4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
    const LOCAL6: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0xff, 0xfe00, 1);
    const PEER6: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
    fn poll(net: &crate::namespace::Namespace, ms: i64) {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    }
    fn input(device: &WireDevice, frame: Vec<u8>) {
        device.wire.lock().unwrap().0.push_back(frame);
    }
    fn frames(device: &WireDevice) -> Vec<Vec<u8>> {
        core::mem::take(&mut device.wire.lock().unwrap().1)
    }
    fn domain() -> (Arc<crate::namespace::Namespace>, Arc<WireDevice>) {
        let net = crate::namespace::Namespace::create().unwrap();
        let device = Arc::new(WireDevice {
            checksum: true,
            ..WireDevice::new()
        });
        net.install_protocol(
            prepare_network(
                &net,
                &[device.clone() as Arc<dyn device_model::network::NetworkDevice>],
                true,
            )
            .unwrap(),
        );
        configure_address(&net, 2, LOCAL4).unwrap();
        configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, 2, true).unwrap();
        for ms in [0, 1000, 2000, 2001] {
            poll(&net, ms);
        }
        frames(&device);
        (net, device)
    }
    fn socket(net: &Arc<crate::namespace::Namespace>, ipv6: bool, raw: bool) -> Arc<InetSocket> {
        InetSocket::new(
            if ipv6 {
                network_protocol::InetFamily::Ipv6
            } else {
                network_protocol::InetFamily::Ipv4
            },
            if raw {
                InetProtocol::Raw(17)
            } else {
                InetProtocol::Udp
            },
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap()
    }
    fn udp(ipv6: bool, payload: &[u8], destination_options: bool) -> Vec<u8> {
        let (source, destination): (IpAddr, IpAddr) = if ipv6 {
            (PEER6.into(), LOCAL6.into())
        } else {
            (PEER4.into(), LOCAL4.into())
        };
        let header = if ipv6 {
            40 + if destination_options { 8 } else { 0 }
        } else {
            20
        };
        let mut bytes = vec![0; header + 8 + payload.len()];
        bytes[header + 8..].copy_from_slice(payload);
        packet_wire::udp::emit(&mut bytes[header..], source, destination, 44000, 44333).unwrap();
        if ipv6 {
            if destination_options {
                bytes[40..48].copy_from_slice(&[17, 0, 0, 0, 0, 0, 0, 0]);
            }
            Ipv6Header {
                source: PEER6,
                destination: LOCAL6,
                next_header: if destination_options { 60 } else { 17 },
                hop_limit: 64,
                traffic_class: 2,
                flow_label: 7,
            }
            .emit(&mut bytes)
            .unwrap();
        } else {
            Ipv4Header {
                source: PEER4,
                destination: LOCAL4,
                protocol: 17,
                hop_limit: 64,
                dscp_ecn: 2,
                identification: 1,
                fragment: 0,
                options: &[],
            }
            .emit(&mut bytes)
            .unwrap();
        }
        bytes
    }
    fn envelope(ip: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; 14 + ip.len()];
        frame[14..].copy_from_slice(ip);
        EthernetHeader {
            source: [2, 0, 0, 0, 0, 2],
            destination: [2, 0, 0, 0, 0, 1],
            type_or_length: if ip[0] >> 4 == 4 { 0x0800 } else { 0x86dd },
            tags: &[],
        }
        .emit(&mut frame)
        .unwrap();
        frame
    }
    fn fragment(packet: &[u8], offset: usize, length: usize, more: bool, id: u32) -> Vec<u8> {
        let v6 = packet[0] >> 4 == 6;
        let base = if v6 { 40 } else { 20 };
        let header = if v6 {
            if offset == 0 { 56 } else { 48 }
        } else {
            20
        };
        let mut ip = vec![0; header + length];
        ip[header..].copy_from_slice(&packet[base + offset..base + offset + length]);
        if v6 {
            let at = if offset == 0 { 48 } else { 40 };
            if offset == 0 {
                ip[40..48].copy_from_slice(&[44, 0, 0, 0, 0, 0, 0, 0]);
            }
            packet_wire::ipv6::Fragment {
                next_header: packet[6],
                offset,
                more,
                identification: id,
            }
            .emit(&mut ip[at..at + 8])
            .unwrap();
            Ipv6Header {
                source: PEER6,
                destination: LOCAL6,
                next_header: if offset == 0 { 0 } else { 44 },
                hop_limit: 64,
                traffic_class: 2,
                flow_label: 7,
            }
            .emit(&mut ip)
            .unwrap();
        } else {
            Ipv4Header {
                source: PEER4,
                destination: LOCAL4,
                protocol: packet[9],
                hop_limit: 64,
                dscp_ecn: packet[1],
                identification: id as u16,
                fragment: (offset / 8) as u16 | if more { 0x2000 } else { 0 },
                options: &[],
            }
            .emit(&mut ip)
            .unwrap();
        }
        envelope(&ip)
    }
    fn bind(receiver: &InetSocket, ipv6: bool) {
        receiver
            .bind(InetAddress::new(
                if ipv6 {
                    Ipv6Addr::UNSPECIFIED.into()
                } else {
                    Ipv4Addr::UNSPECIFIED.into()
                },
                44333,
            ))
            .unwrap();
        if ipv6 {
            receiver
                .set_ipv6_option(InternetOption::Ipv6PacketInfo, 1)
                .unwrap();
        } else {
            receiver.set_packet_info(true).unwrap();
        }
    }
    fn learn(net: &crate::namespace::Namespace, device: &WireDevice, at: i64) {
        input(device, peer_arp_frame_for(LOCAL4, PEER4));
        let mut ip = vec![0; 72];
        ip[64..72].copy_from_slice(&[1, 1, 2, 0, 0, 0, 0, 2]);
        packet_wire::ndisc::NdMessage::NeighborSolicitation { target: LOCAL6 }
            .emit(&mut ip[40..], PEER6, LOCAL6)
            .unwrap();
        Ipv6Header {
            source: PEER6,
            destination: LOCAL6,
            next_header: 58,
            hop_limit: 255,
            traffic_class: 0,
            flow_label: 0,
        }
        .emit(&mut ip)
        .unwrap();
        input(device, envelope(&ip));
        poll(net, at);
        frames(device);
    }
    #[test]
    fn reassembly_delivers_large_dual_stack_udp_raw_once_and_checks_complete_checksum() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        for v6 in [false, true] {
            let receiver = socket(&net, v6, false);
            bind(&receiver, v6);
            let raw = socket(&net, v6, true);
            let payload = vec![0x57; 3000];
            let packet = udp(v6, &payload, v6);
            let full = packet.len() - if v6 { 40 } else { 20 };
            let mut last = fragment(&packet, 2048, full - 2048, false, 10);
            // CE 在一个后续片出现，完整包必须保留拥塞标记。
            if v6 {
                last[15] |= 0x30;
            } else {
                last[15] |= 3;
                last[24..26].fill(0);
                let value = packet_wire::checksum::checksum(&last[14..34]);
                last[24..26].copy_from_slice(&value.to_be_bytes());
            }
            input(&device, last);
            input(&device, fragment(&packet, 0, 1024, true, 10));
            input(&device, fragment(&packet, 0, 1024, true, 10));
            poll(&net, 3000);
            assert!(!receiver.poll_state().readable && !raw.poll_state().readable);
            input(&device, fragment(&packet, 1024, 1024, true, 10));
            poll(&net, 3001);
            let mut bytes = vec![0; 4096];
            let got = receive(&receiver, &mut bytes, false).unwrap();
            assert_eq!(&bytes[..got.0], payload.as_slice());
            if v6 {
                assert_eq!(
                    got.3,
                    Some(InternetPacketInfo::Ipv6 {
                        interface_index: 2,
                        destination: LOCAL6
                    })
                );
            } else {
                assert_eq!(
                    got.3,
                    Some(InternetPacketInfo::Ipv4 {
                        interface_index: 2,
                        specific_address: LOCAL4,
                        destination: LOCAL4
                    })
                );
            }
            let got = receive(&raw, &mut bytes, false).unwrap();
            if v6 {
                assert_eq!(&bytes[8..got.0], payload.as_slice());
            } else {
                let ip = Ipv4Packet::parse(&bytes[..got.0]).unwrap();
                assert!(!ip.fragmented());
                assert_eq!(ip.bytes()[1] & 3, 3);
                assert_eq!(&ip.payload()[8..], payload.as_slice());
            }
            assert!(!receiver.poll_state().readable && !raw.poll_state().readable);
            // 分片级卸载标志不能替代重组后完整 UDP checksum。
            device
                .receive_checksum_valid
                .store(true, core::sync::atomic::Ordering::Relaxed);
            let mut invalid = packet.clone();
            *invalid.last_mut().unwrap() ^= 1;
            input(&device, fragment(&invalid, 0, 1024, true, 11));
            input(&device, fragment(&invalid, 1024, 1024, true, 11));
            input(&device, fragment(&invalid, 2048, full - 2048, false, 11));
            poll(&net, 3002);
            assert!(!receiver.poll_state().readable);
            device
                .receive_checksum_valid
                .store(false, core::sync::atomic::Ordering::Relaxed);
            receiver.close_description();
            raw.close_description();
        }
    }
    #[test]
    fn reassembly_rejects_overlap_conflicting_ends_and_mixed_ecn_and_cleans_after_failure() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        for v6 in [false, true] {
            let receiver = socket(&net, v6, false);
            bind(&receiver, v6);
            let packet = udp(v6, &[9; 40], false);
            input(&device, fragment(&packet, 0, 24, true, 21));
            input(&device, fragment(&packet, 16, 32, false, 21));
            poll(&net, 3000);
            assert!(!receiver.poll_state().readable);
            input(&device, fragment(&packet, 0, 24, true, 22));
            let mut changed = fragment(&packet, 0, 24, true, 22);
            *changed.last_mut().unwrap() ^= 1;
            input(&device, changed);
            input(&device, fragment(&packet, 24, 24, false, 22));
            poll(&net, 3001);
            assert!(!receiver.poll_state().readable);
            // OOM 不留下半个成员或不可回收存储；相同标识的新完整尝试仍可成功。
            input(&device, fragment(&packet, 0, 24, true, 23));
            crate::allocation_failure::fail_next();
            poll(&net, 3002);
            assert!(crate::allocation_failure::was_consumed());
            input(&device, fragment(&packet, 24, 24, false, 23));
            poll(&net, 3003);
            assert!(!receiver.poll_state().readable);
            input(&device, fragment(&packet, 0, 24, true, 23));
            poll(&net, 3004);
            let mut bytes = [0; 64];
            assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 40);
            // 两个互不重叠的片不能给出不同的末尾；冲突后不能借补洞复活旧数据。
            input(&device, fragment(&packet, 40, 8, false, 25));
            input(&device, fragment(&packet, 24, 16, false, 25));
            input(&device, fragment(&packet, 0, 24, true, 25));
            input(&device, fragment(&packet, 24, 16, true, 25));
            poll(&net, 3005);
            assert!(!receiver.poll_state().readable);
            // Not-ECT 与 ECT 的混合不能生成一个合法完整包。
            let mut last = fragment(&packet, 24, 24, false, 24);
            if v6 {
                last[15] &= !0x30;
            } else {
                last[15] &= !3;
                last[24..26].fill(0);
                let value = packet_wire::checksum::checksum(&last[14..34]);
                last[24..26].copy_from_slice(&value.to_be_bytes());
            }
            input(&device, fragment(&packet, 0, 24, true, 24));
            input(&device, last);
            poll(&net, 3005);
            assert!(!receiver.poll_state().readable);
            receiver.close_description();
        }
    }
    #[test]
    fn reassembly_timeout_uses_original_first_fragment_and_does_not_reset_on_duplicate() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        let deadline = || stack(&net).unwrap().lock().unwrap().reassembly.next_deadline();
        learn(&net, &device, 2002);
        for (v6, id, at) in [(false, 31, 3000), (true, 32, 64000)] {
            let packet = udp(v6, &[9; 40], false);
            input(&device, fragment(&packet, 0, 24, true, id));
            poll(&net, at);
            input(&device, fragment(&packet, 0, 24, true, id));
            poll(&net, at + 59000);
            learn(&net, &device, at + 59000);
            assert_eq!(deadline(), Some(Instant::from_millis(at + 60000)));
            poll(&net, at + 60000);
            poll(&net, at + 60001);
            assert_eq!(deadline(), None);
            let output = frames(&device);
            let errors: Vec<_> = output
                .iter()
                .filter(|frame| {
                    let ethernet = EthernetFrame::parse(frame).unwrap();
                    if v6 {
                        ethernet.type_or_length() == 0x86dd
                            && Ipv6Packet::parse(ethernet.payload())
                                .unwrap()
                                .upper_protocol()
                                == 58
                            && Ipv6Packet::parse(ethernet.payload())
                                .unwrap()
                                .upper_layer()
                                .unwrap()[0]
                                == 3
                    } else {
                        ethernet.type_or_length() == 0x0800
                            && Ipv4Packet::parse(ethernet.payload()).unwrap().protocol() == 1
                    }
                })
                .collect();
            assert_eq!(
                errors.len(),
                1,
                "first fragment timeout must produce one bounded ICMP error"
            );
            let ethernet = EthernetFrame::parse(errors[0]).unwrap();
            if v6 {
                let ip = Ipv6Packet::parse(ethernet.payload()).unwrap();
                let icmp = packet_wire::icmpv6::Icmpv6Packet::parse(
                    ip.upper_layer().unwrap(),
                    LOCAL6,
                    PEER6,
                )
                .unwrap();
                assert_eq!((icmp.message_type(), icmp.code()), (3, 1));
                assert_eq!(
                    icmp.body()[4 + 48],
                    17,
                    "quote preserves the original Fragment header"
                );
            } else {
                let ip = Ipv4Packet::parse(ethernet.payload()).unwrap();
                let icmp = packet_wire::icmpv4::Icmpv4Packet::parse(ip.payload()).unwrap();
                assert_eq!((icmp.message_type(), icmp.code()), (11, 1));
            }
        }
        let receiver = socket(&net, false, false);
        bind(&receiver, false);
        let packet = udp(false, &[9; 40], false);
        input(&device, fragment(&packet, 0, 24, true, 33));
        poll(&net, 124002);
        assert!(deadline().is_some());
        configure_up(&net, 2, false).unwrap();
        configure_up(&net, 2, true).unwrap();
        assert_eq!(deadline(), None);
        input(&device, fragment(&packet, 24, 24, false, 33));
        poll(&net, 124003);
        assert!(!receiver.poll_state().readable);
        input(&device, fragment(&packet, 0, 24, true, 33));
        poll(&net, 124004);
        assert!(receiver.poll_state().readable);
        assert_eq!(deadline(), None);
        receiver.close_description();
    }
    #[test]
    fn reassembly_ipv6_atomic_packets_are_independent_and_incomplete_first_headers_get_errors() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        learn(&net, &device, 2002);
        let receiver = socket(&net, true, false);
        bind(&receiver, true);
        let packet = udp(true, &[5; 40], false);
        input(&device, fragment(&packet, 0, 24, true, 40));
        input(&device, fragment(&packet, 0, 48, false, 40));
        poll(&net, 3000);
        let mut bytes = [0; 100];
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 40);
        input(&device, fragment(&packet, 24, 24, false, 40));
        poll(&net, 3001);
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 40);
        // TCP 首片必须包含整个 TCP header；不能等后续片补齐才能识别连接。
        let mut incomplete = packet.clone();
        incomplete[6] = 6;
        input(&device, fragment(&incomplete, 0, 8, true, 41));
        poll(&net, 3002);
        poll(&net, 3003);
        let output = frames(&device);
        assert!(output.iter().any(|frame| {
            let ethernet = EthernetFrame::parse(frame).unwrap();
            if ethernet.type_or_length() != 0x86dd {
                return false;
            }
            let ip = Ipv6Packet::parse(ethernet.payload()).unwrap();
            ip.upper_protocol() == 58 && ip.upper_layer().unwrap()[..2] == [4, 3]
        }));
        // 即使重组已删除 Fragment header，也不能让 ND 学习伪造来源链路地址。
        let mut nd = vec![0; 72];
        nd[64..72].copy_from_slice(&[1, 1, 2, 0, 0, 0, 0, 9]);
        packet_wire::ndisc::NdMessage::NeighborSolicitation { target: LOCAL6 }
            .emit(&mut nd[40..], PEER6, LOCAL6)
            .unwrap();
        Ipv6Header {
            source: PEER6,
            destination: LOCAL6,
            next_header: 58,
            hop_limit: 255,
            traffic_class: 2,
            flow_label: 0,
        }
        .emit(&mut nd)
        .unwrap();
        for (offset, length, more) in [(0, 24, true), (24, 8, false)] {
            let mut frame = fragment(&nd, offset, length, more, 42);
            frame[21] = 255;
            input(&device, frame);
        }
        poll(&net, 3004);
        poll(&net, 3005);
        assert!(!frames(&device).iter().any(|frame| {
            let ethernet = EthernetFrame::parse(frame).unwrap();
            if ethernet.type_or_length() != 0x86dd {
                return false;
            }
            let ip = Ipv6Packet::parse(ethernet.payload()).unwrap();
            ip.upper_protocol() == 58 && ip.upper_layer().unwrap()[0] == 136
        }));
        receiver.close_description();
    }
    // 实际 NIC submit 背压和跨轮预算不能由头部格式检查证明；同时让真实接收 owner 验证最终数据。
    #[test]
    fn ipv4_fragmented_udp_freezes_policy_and_resumes_only_after_committed_bytes() {
        use core::sync::atomic::Ordering;
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        learn(&net, &device, 2002);
        configure_mtu(&net, 2, 68).unwrap();
        frames(&device);
        let sender = socket(&net, false, false);
        assert_eq!(sender.option(InternetOption::Ipv4MtuDiscovery), Ok(1));
        sender.set_ipv4_mtu_discovery(2).unwrap();
        let target = Some(InetAddress::new(PEER4.into(), 44333));
        assert_eq!(
            sender.send_to(&[7; 100], target, &[]),
            Err(SocketError::MessageTooLarge)
        );
        sender.set_ipv4_mtu_discovery(1).unwrap();
        let payload = vec![0x63; 6000];
        sender.send_to(&payload, target, &[]).unwrap();
        poll(&net, 3000);
        let mut output = frames(&device);
        assert_eq!(output.len(), 64);
        device.wire.lock().unwrap().2.submit_error =
            Some(device_model::network::NetworkError::WouldBlock);
        poll(&net, 3001);
        assert!(frames(&device).is_empty());
        sender.set_ipv4_mtu_discovery(2).unwrap();
        poll(&net, 5000);
        output.extend(frames(&device));
        assert_eq!(output.len(), 126);
        let id = Ipv4Packet::parse(EthernetFrame::parse(&output[0]).unwrap().payload())
            .unwrap()
            .identification();
        assert_ne!(id, 0);
        let mut offset = 0;
        for (index, frame) in output.iter().enumerate() {
            let ip = Ipv4Packet::parse(EthernetFrame::parse(frame).unwrap().payload()).unwrap();
            assert!(ip.bytes().len() <= 68);
            assert_eq!(ip.identification(), id);
            assert_eq!(ip.fragment_offset(), offset);
            assert_eq!(ip.more_fragments(), index + 1 < output.len());
            assert_eq!(ip.bytes()[6] & 0x40, 0);
            offset += ip.payload().len();
        }
        assert_eq!(offset, payload.len() + 8);
        let (receiver_net, receiver_device) = domain();
        configure_address(&receiver_net, 2, PEER4).unwrap();
        let receiver = socket(&receiver_net, false, false);
        bind(&receiver, false);
        for mut frame in output {
            frame[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
            input(&receiver_device, frame);
        }
        for ms in 5001..5005 {
            poll(&receiver_net, ms);
        }
        let mut bytes = vec![0; 6100];
        let got = receive(&receiver, &mut bytes, false).unwrap();
        assert_eq!(&bytes[..got.0], payload.as_slice());
        // 普通报文仍走 NIC 校验卸载；真正 submit 失败不能虚报交付或丢弃队首。
        device.wire.lock().unwrap().2.submit_error =
            Some(device_model::network::NetworkError::WouldBlock);
        sender.send_to(b"small", target, &[]).unwrap();
        poll(&net, 6000);
        assert!(frames(&device).is_empty());
        poll(&net, 8000);
        let small = frames(&device);
        assert_eq!(small.len(), 1);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&small[0]).unwrap().payload()).unwrap();
        assert!(!ip.fragmented());
        assert_ne!(ip.bytes()[6] & 0x40, 0);
        assert_eq!(
            packet_wire::udp::UdpPacket::parse(ip.payload(), LOCAL4.into(), PEER4.into())
                .unwrap()
                .payload(),
            b"small"
        );
        assert_eq!(device.cancelled_queue.load(Ordering::Relaxed), 0);

        // 本轮截止已过，仍保证一次提交。端点刚好清空时，下一个被拒绝的端点应在续轮先获得机会。
        let short_sender = socket(&net, false, false);
        short_sender.send_to(b"first", target, &[]).unwrap();
        sender.set_ipv4_mtu_discovery(1).unwrap();
        sender.send_to(&[0x64; 100], target, &[]).unwrap();
        let expired_round = |millis| {
            stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .poll_until(Instant::from_millis(millis), 0)
        };
        assert!(expired_round(8001).backlog);
        let first = frames(&device);
        assert_eq!(first.len(), 1);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&first[0]).unwrap().payload()).unwrap();
        assert!(!ip.fragmented());
        short_sender.send_to(b"next", target, &[]).unwrap();
        assert!(expired_round(8002).backlog);
        let continued = frames(&device);
        assert_eq!(continued.len(), 1);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&continued[0]).unwrap().payload()).unwrap();
        assert_eq!(ip.fragment_offset(), 0);
        assert!(ip.more_fragments());
        poll(&net, 8003);
        assert_eq!(frames(&device).len(), 3);
        short_sender.close_description();
        sender.close_description();
        receiver.close_description();
    }
    #[test]
    fn ipv4_raw_fragmentation_and_user_fragment_headers_follow_distinct_standard_semantics() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        learn(&net, &device, 2002);
        configure_mtu(&net, 2, 68).unwrap();
        frames(&device);
        let sender = scoped_endpoint(&net, InetProtocol::Raw(89));
        let target = Some(InetAddress::new(PEER4.into(), 0));
        sender.set_ipv4_mtu_discovery(0).unwrap();
        sender.send_to(&[9; 100], target, &[]).unwrap();
        device.wire.lock().unwrap().2.submit_error =
            Some(device_model::network::NetworkError::WouldBlock);
        poll(&net, 3000);
        assert!(frames(&device).is_empty());
        poll(&net, 5000);
        let output = frames(&device);
        assert_eq!(output.len(), 3);
        for (index, frame) in output.iter().enumerate() {
            let ip = Ipv4Packet::parse(EthernetFrame::parse(frame).unwrap().payload()).unwrap();
            assert_eq!(ip.fragment_offset(), index * 48);
            assert_eq!(
                ip.payload(),
                &[9; 100][index * 48..(index * 48 + 48).min(100)]
            );
        }
        sender.set_header_included(true).unwrap();
        let mut first = vec![0; 56];
        first[24..].fill(0x37);
        Ipv4Header {
            source: Ipv4Addr::UNSPECIFIED,
            destination: PEER4,
            protocol: 89,
            hop_limit: 17,
            dscp_ecn: 0,
            identification: 0x7654,
            fragment: 0x2000,
            options: &[0x94, 4, 0, 0],
        }
        .emit(&mut first)
        .unwrap();
        sender.send_to(&first, target, &[]).unwrap();
        poll(&net, 5001);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&output[0]).unwrap().payload()).unwrap();
        assert_eq!(
            (ip.source(), ip.hop_limit(), ip.identification()),
            (LOCAL4, 17, 0x7654)
        );
        assert!(ip.more_fragments());
        assert_eq!(ip.options(), &[0x94, 4, 0, 0]);
        let mut oversized = first.clone();
        oversized.resize(80, 0);
        assert_eq!(
            sender.send_to(&oversized, target, &[]),
            Err(SocketError::MessageTooLarge)
        );
        first[21] = 1;
        assert_eq!(
            sender.send_to(&first, target, &[]),
            Err(SocketError::Invalid)
        );
        // 自带的组播分片回送仍按逻辑物理接口重组，而不是误查 lo 的组表。
        let group = Ipv4Addr::new(239, 1, 1, 9);
        let receiver = scoped_endpoint(&net, InetProtocol::Raw(89));
        receiver.set_ipv4_multicast_all(false).unwrap();
        receiver.ipv4_membership(group, 2, LOCAL4, true).unwrap();
        sender
            .set_ipv4_multicast_interface(2, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        let mut group_first = vec![0; 56];
        group_first[24..].fill(0x37);
        Ipv4Header {
            source: Ipv4Addr::UNSPECIFIED,
            destination: group,
            protocol: 89,
            hop_limit: 17,
            dscp_ecn: 0,
            identification: 0x7655,
            fragment: 0x2000,
            options: &[0x94, 4, 0, 0],
        }
        .emit(&mut group_first)
        .unwrap();
        let mut group_last = group_first[..40].to_vec();
        group_last[6..8].copy_from_slice(&4u16.to_be_bytes());
        let target = Some(InetAddress::new(group.into(), 0));
        sender.send_to(&group_first, target, &[]).unwrap();
        sender.send_to(&group_last, target, &[]).unwrap();
        for ms in 6000..6005 {
            poll(&net, ms);
        }
        let mut bytes = [0; 100];
        let got = receive(&receiver, &mut bytes, false).unwrap();
        let ip = Ipv4Packet::parse(&bytes[..got.0]).unwrap();
        assert_eq!(ip.payload(), &[0x37; 48]);
        assert!(!ip.fragmented());
        receiver.close_description();
        sender.close_description();
    }
    // 实际 TX 提交、冻结的 socket 上限和接收端重组横跨设备及 UDP/RAW，静态审查无法覆盖。
    #[test]
    fn ipv6_fragmented_udp_and_raw_use_shared_device_and_reassembly_paths() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        learn(&net, &device, 2002);
        let target = Some(InetAddress::V6(core::net::SocketAddrV6::new(
            PEER6, 44333, 0, 2,
        )));
        let sender = socket(&net, true, false);
        sender
            .set_ipv6_option(InternetOption::Ipv6Mtu, 1280)
            .unwrap();
        assert_eq!(sender.option(InternetOption::Ipv6DontFragment), Ok(0));
        let payload = vec![0xa7; 3001];
        sender.send_to(&payload, target, &[]).unwrap();
        sender
            .set_ipv6_option(InternetOption::Ipv6DontFragment, 1)
            .unwrap();
        assert_eq!(
            sender.send_to(&payload, target, &[]),
            Err(SocketError::MessageTooLarge)
        );
        device.wire.lock().unwrap().2.submit_error =
            Some(device_model::network::NetworkError::WouldBlock);
        poll(&net, 3000);
        assert!(frames(&device).is_empty());
        poll(&net, 4000);
        let output = frames(&device);
        assert_eq!(output.len(), 3);
        let receiver = socket(&net, true, false);
        bind(&receiver, true);
        let mut offset = 0;
        let mut id = None;
        for (index, frame) in output.iter().enumerate() {
            let bytes = EthernetFrame::parse(frame).unwrap().payload();
            let ip = Ipv6Packet::parse(bytes).unwrap();
            let fragment = ip.extensions().next().unwrap().fragment().unwrap();
            assert!(bytes.len() <= 1280);
            assert_eq!(fragment.fragment_offset(), offset);
            assert_eq!(fragment.more(), index + 1 < output.len());
            assert_eq!(
                *id.get_or_insert_with(|| fragment.identification()),
                fragment.identification()
            );
            offset += bytes.len() - 48;
            // 交换地址重放到本机，UDP 伪头的地址之和保持，仍由真实接收路径验算。
            let mut reversed = bytes.to_vec();
            reversed[8..24].copy_from_slice(&PEER6.octets());
            reversed[24..40].copy_from_slice(&LOCAL6.octets());
            input(&device, envelope(&reversed));
        }
        assert_eq!(offset, payload.len() + 8);
        poll(&net, 4001);
        let mut received = vec![0; payload.len()];
        let got = receive(&receiver, &mut received, false).unwrap();
        assert_eq!(&received[..got.0], payload.as_slice());

        let raw = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Raw(60),
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        raw.set_ipv6_option(InternetOption::Ipv6Mtu, 1280).unwrap();
        let mut data = vec![0x37; 1600];
        data[..8].copy_from_slice(&[17, 0, 1, 4, 0, 0, 0, 0]);
        packet_wire::udp::emit(&mut data[8..], LOCAL6.into(), PEER6.into(), 44300, 44333).unwrap();
        let raw_target = Some(InetAddress::V6(core::net::SocketAddrV6::new(
            PEER6, 0, 0, 2,
        )));
        raw.send_to(&data, raw_target, &[]).unwrap();
        poll(&net, 4002);
        let output = frames(&device);
        assert_eq!(output.len(), 2);
        let first = EthernetFrame::parse(&output[0]).unwrap().payload();
        let ip = Ipv6Packet::parse(first).unwrap();
        assert_eq!(
            ip.extensions().map(|ext| ext.kind()).collect::<Vec<_>>(),
            [44, 60]
        );
        let raw_id = ip
            .extensions()
            .next()
            .unwrap()
            .fragment()
            .unwrap()
            .identification();
        assert_ne!(Some(raw_id), id);
        raw.set_header_included(true).unwrap();
        raw.send_to(first, raw_target, &[]).unwrap();
        poll(&net, 4003);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_eq!(EthernetFrame::parse(&output[0]).unwrap().payload(), first);
        raw.close_description();
        receiver.close_description();
        sender.close_description();
    }

    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ip_feedback_tests.rs"
    ));

    #[test]
    fn ipv4_small_mtu_icmp_replies_fragment_without_forwarding_enabled() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        stack(&net).unwrap().lock().unwrap().forwarding_enabled.ipv4 = false;
        learn(&net, &device, 2002);
        configure_mtu(&net, 2, 68).unwrap();
        frames(&device);
        let request = peer_udp_frame_for(
            InetAddress::new(PEER4.into(), 44000),
            InetAddress::new(LOCAL4.into(), 44999),
            &[8; 100],
        );
        input(&device, request);
        let mut output = Vec::new();
        for ms in 3000..3010 {
            poll(&net, ms);
            output.extend(frames(&device));
        }
        let packets: Vec<_> = output
            .iter()
            .filter_map(|frame| {
                let ethernet = EthernetFrame::parse(frame).unwrap();
                (ethernet.type_or_length() == 0x0800)
                    .then(|| Ipv4Packet::parse(ethernet.payload()).unwrap())
            })
            .collect();
        assert!(packets.len() > 1);
        let id = packets[0].identification();
        assert_ne!(id, 0);
        let mut body = Vec::new();
        for ip in &packets {
            assert_eq!(ip.protocol(), 1);
            assert_eq!(ip.identification(), id);
            assert!(ip.bytes().len() <= 68);
            assert_eq!(ip.fragment_offset(), body.len());
            body.extend_from_slice(ip.payload());
        }
        let icmp = packet_wire::icmpv4::Icmpv4Packet::parse(&body).unwrap();
        assert_eq!((icmp.message_type(), icmp.code()), (3, 3));

        let mut echo = peer_icmp_frame_for(PEER4, LOCAL4);
        let payload_offset = packet_wire::ethernet::HEADER_LENGTH + 20;
        packet_wire::icmpv4::emit(&mut echo[payload_offset..], 8, 0).unwrap();
        let echoed = echo[payload_offset + 4..].to_vec();
        input(&device, echo);
        poll(&net, 3010);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        let ethernet = EthernetFrame::parse(&output[0]).unwrap();
        let ip = Ipv4Packet::parse(ethernet.payload()).unwrap();
        assert_eq!(
            (ip.source(), ip.destination(), ip.protocol()),
            (LOCAL4, PEER4, 1),
        );
        let reply = packet_wire::icmpv4::Icmpv4Packet::parse(ip.payload()).unwrap();
        assert_eq!((reply.message_type(), reply.code()), (0, 0));
        assert_eq!(&reply.bytes()[4..], echoed);
    }
}
