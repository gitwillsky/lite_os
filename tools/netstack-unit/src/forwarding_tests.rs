// 真正从 NIC A 收到非本机包，由生产 NET/FIB/ARP 从 NIC B 发出；本机 socket 用例无法覆盖这个契约。
mod ipv4_forwarding {
    use super::*;
    use packet_wire::{
        ethernet::{EthernetFrame, EthernetHeader},
        icmpv4,
        ipv4::{Ipv4Header, Ipv4Packet},
    };
    pub(super) fn datagram(ttl: u8, fragment: u16, options: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 14 + 20 + options.len() + payload.len()];
        EthernetHeader {
            source: [2, 0, 0, 0, 0, 2],
            destination: [2, 0, 0, 0, 0, 1],
            type_or_length: 0x0800,
            tags: &[],
        }
        .emit(&mut bytes)
        .unwrap();
        bytes[34 + options.len()..].copy_from_slice(payload);
        Ipv4Header {
            source: Ipv4Addr::new(10, 40, 0, 9),
            destination: Ipv4Addr::new(10, 41, 0, 9),
            protocol: 253,
            hop_limit: ttl,
            dscp_ecn: 0x2e,
            identification: 712,
            fragment,
            options,
        }
        .emit(&mut bytes[14..])
        .unwrap();
        bytes
    }
    fn poll(net: &Arc<crate::namespace::Namespace>, ms: i64) {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    }
    fn packets(device: &Arc<WireDevice>) -> Vec<Vec<u8>> {
        let mut wire = device.wire.lock().unwrap();
        wire.1
            .drain(..)
            .filter_map(|frame| {
                let ethernet = EthernetFrame::parse(&frame).unwrap();
                (ethernet.type_or_length() == 0x0800).then(|| ethernet.payload().to_vec())
            })
            .collect()
    }
    #[test]
    fn cross_nic_transit_keeps_headers_fragments_and_errors_on_the_actual_wire() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, a, b) = routing_domain(89401);
        a.wire.lock().unwrap().0.push_back(peer_arp_frame_for(
            Ipv4Addr::new(10, 40, 0, 15),
            Ipv4Addr::new(10, 40, 0, 9),
        ));
        b.wire.lock().unwrap().0.push_back(peer_arp_frame_for(
            Ipv4Addr::new(10, 41, 0, 15),
            Ipv4Addr::new(10, 41, 0, 9),
        ));
        poll(&net, 1000);
        packets(&a);
        packets(&b);
        let bytes = datagram(64, 0, &[], b"native-transit-without-local-socket");
        a.wire.lock().unwrap().0.push_back(bytes.clone());
        poll(&net, 1001);
        let output = packets(&b);
        assert_eq!(output.len(), 1);
        let ip = Ipv4Packet::parse(&output[0]).unwrap();
        assert_eq!(ip.hop_limit(), 63);
        assert_eq!(ip.identification(), 712);
        assert_eq!(ip.bytes()[1], 0x2e);
        assert_eq!(ip.payload(), b"native-transit-without-local-socket");
        assert!(packets(&a).is_empty());
        // TTL 错误引用减 TTL 之前的原包；不继承正向出口。
        let expired = datagram(1, 0, &[], b"expired");
        a.wire.lock().unwrap().0.push_back(expired.clone());
        poll(&net, 1002);
        let errors = packets(&a);
        assert_eq!(errors.len(), 1);
        let ip = Ipv4Packet::parse(&errors[0]).unwrap();
        let error = icmpv4::Icmpv4Packet::parse(ip.payload()).unwrap();
        assert_eq!((error.message_type(), error.code()), (11, 0));
        assert_eq!(&error.body()[4..], &expired[14..]);
        // 出口 MTU 下的分片保持标识、偏移、MF、payload 与 copied 选项；DF 则报告真实 MTU。
        configure_mtu(&net, 3, 100).unwrap();
        let options = [0x94, 4, 0, 0, 7, 7, 4, 0, 0, 0, 0, 0];
        let payload: Vec<u8> = (0..200).map(|index| index as u8).collect();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(7, 0, &options, &payload));
        poll(&net, 1003);
        let output = packets(&b);
        assert_eq!(output.len(), 4);
        let mut restored = Vec::new();
        for (index, bytes) in output.iter().enumerate() {
            let ip = Ipv4Packet::parse(bytes).unwrap();
            assert_eq!(ip.hop_limit(), 6);
            assert_eq!(ip.identification(), 712);
            assert_eq!(ip.fragment_offset(), restored.len());
            assert_eq!(ip.more_fragments(), index != 3);
            assert_eq!(&ip.options()[..4], &[0x94, 4, 0, 0]);
            if index == 0 {
                assert_eq!(&ip.options()[7..11], &[10, 41, 0, 15]);
            } else {
                assert!(ip.options()[4..11].iter().all(|byte| *byte == 1));
            }
            restored.extend_from_slice(ip.payload());
        }
        assert_eq!(restored, payload);
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(5, 0x4000, &[], &payload));
        poll(&net, 1004);
        let errors = packets(&a);
        let ip = Ipv4Packet::parse(&errors[0]).unwrap();
        let error = icmpv4::Icmpv4Packet::parse(ip.payload()).unwrap();
        assert_eq!((error.message_type(), error.code()), (3, 4));
        assert_eq!(&error.body()[..4], &[0, 0, 0, 100]);
        assert!(packets(&b).is_empty());
        // 源路由默认关闭；坏校验和、非本机 MAC、广播源和 ICMP 错误不触发错误反射。
        a.wire.lock().unwrap().0.push_back(datagram(
            5,
            0,
            &[131, 7, 4, 10, 41, 0, 9, 0],
            b"source-route",
        ));
        poll(&net, 1005);
        let errors = packets(&a);
        let ip = Ipv4Packet::parse(&errors[0]).unwrap();
        let error = icmpv4::Icmpv4Packet::parse(ip.payload()).unwrap();
        assert_eq!((error.message_type(), error.code()), (3, 5));
        let mut bad = bytes.clone();
        bad[24] ^= 1;
        a.wire.lock().unwrap().0.push_back(bad);
        let mut other_mac = bytes.clone();
        other_mac[5] = 99;
        a.wire.lock().unwrap().0.push_back(other_mac);
        let mut broadcast_source = bytes.clone();
        broadcast_source[26..30].copy_from_slice(&[10, 40, 0, 255]);
        broadcast_source[24..26].fill(0);
        let checksum = packet_wire::checksum::checksum(&broadcast_source[14..34]);
        broadcast_source[24..26].copy_from_slice(&checksum.to_be_bytes());
        a.wire.lock().unwrap().0.push_back(broadcast_source);
        let mut original_error = datagram(1, 0, &[], &[3, 0, 0, 0, 0, 0, 0, 0]);
        original_error[23] = 1;
        original_error[24..26].fill(0);
        let checksum = packet_wire::checksum::checksum(&original_error[14..34]);
        original_error[24..26].copy_from_slice(&checksum.to_be_bytes());
        a.wire.lock().unwrap().0.push_back(original_error);
        poll(&net, 1006);
        assert!(packets(&a).is_empty());
        assert!(packets(&b).is_empty());
    }
    #[test]
    fn unresolved_neighbor_and_tx_backpressure_retry_original_without_resending_fragments() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, a, b) = routing_domain(89402);
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"wait-for-neighbor"));
        poll(&net, 1000);
        assert!(packets(&b).is_empty());
        b.transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        b.wire.lock().unwrap().0.push_back(peer_arp_frame_for(
            Ipv4Addr::new(10, 41, 0, 15),
            Ipv4Addr::new(10, 41, 0, 9),
        ));
        poll(&net, 1001);
        poll(&net, 1002);
        assert!(packets(&b).is_empty());
        b.transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        b.transmit_ready
            .store(true, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 1003);
        let output = packets(&b);
        assert_eq!(output.len(), 1);
        let ip = Ipv4Packet::parse(&output[0]).unwrap();
        assert_eq!(ip.hop_limit(), 8);
        assert_eq!(ip.payload(), b"wait-for-neighbor");
        poll(&net, 1004);
        assert!(packets(&b).is_empty());
        // 细小 MTU 使一个包超过本轮 TX 预算；后续轮次只发送尚未发出的区间，不等待 ARP 重试秒表。
        configure_mtu(&net, 3, 68).unwrap();
        let payload = vec![0x5a; 1400];
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(4, 0, &[1; 40], &payload));
        poll(&net, 1005);
        let mut fragments = packets(&b);
        assert_eq!(fragments.len(), NETWORK_RX_BUDGET);
        for time in 1006..1010 {
            poll(&net, time);
            fragments.extend(packets(&b));
        }
        let mut restored = Vec::new();
        assert_eq!(fragments.len(), 175);
        for bytes in &fragments {
            let ip = Ipv4Packet::parse(bytes).unwrap();
            assert_eq!(ip.fragment_offset(), restored.len());
            assert_eq!(ip.hop_limit(), 3);
            restored.extend_from_slice(ip.payload());
        }
        assert_eq!(restored, payload);
    }
    #[test]
    fn forwarding_switch_uses_the_existing_write_lease_and_preserves_local_traffic() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, a, b) = routing_domain(89403);
        let before = configuration_routes(&net);
        // 未解析的原包留在生产队列中；未确认的关闭必须能撤销，不能提前释放。
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"paused-original"));
        poll(&net, 1000);
        let paused = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: false,
                ipv6: true,
            }),
        )
        .unwrap();
        paused.activate().unwrap();
        assert_eq!(configuration_routes(&net), before);
        assert!(matches!(
            prepare_configuration(
                &net,
                core::iter::empty(),
                Some(network_protocol::Forwarding {
                    ipv4: true,
                    ipv6: true
                })
            ),
            Err(SocketError::Again)
        ));
        b.wire.lock().unwrap().0.push_back(peer_arp_frame_for(
            Ipv4Addr::new(10, 41, 0, 15),
            Ipv4Addr::new(10, 41, 0, 9),
        ));
        poll(&net, 1001);
        assert!(packets(&b).is_empty());
        paused.abort().unwrap();
        poll(&net, 1002);
        let resumed = packets(&b);
        assert_eq!(resumed.len(), 1);
        assert_eq!(Ipv4Packet::parse(&resumed[0]).unwrap().hop_limit(), 8);
        b.transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"discard-on-confirm"));
        poll(&net, 1003);
        let disabled = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: false,
                ipv6: true,
            }),
        )
        .unwrap();
        disabled.activate().unwrap();
        disabled.confirm().unwrap();
        b.transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        b.transmit_ready
            .store(true, core::sync::atomic::Ordering::Relaxed);
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"must-not-forward"));
        poll(&net, 1003);
        assert!(packets(&b).is_empty());
        // 接口/FIB 后续更新未携带开关时保持实际关闭，不能被动态路由或 DHCP 重置。
        let unchanged =
            prepare_configuration(&net, [interface_plan(3, 41)].into_iter(), None).unwrap();
        unchanged.activate().unwrap();
        unchanged.confirm().unwrap();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"still-disabled"));
        poll(&net, 1004);
        assert!(packets(&b).is_empty());
        let local = InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 40, 0, 15),
            45119,
        ));
        let peer = InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(10, 40, 0, 9),
            45120,
        ));
        let endpoint = scoped_endpoint(&net, InetProtocol::Udp);
        endpoint.bind(local).unwrap();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(peer_udp_frame_for(peer, local, b"local-still-works"));
        poll(&net, 1005);
        assert_eq!(udp_read(&endpoint, false).unwrap().0, b"local-still-works");
        endpoint.close_description();
        let invalid = router_protocol::interface::InterfaceConfiguration {
            ipv6: None,
            index: u32::MAX,
            ..interface_plan(3, 41)
        };
        assert!(matches!(
            prepare_configuration(
                &net,
                [invalid].into_iter(),
                Some(network_protocol::Forwarding {
                    ipv4: true,
                    ipv6: true
                })
            ),
            Err(SocketError::NoDevice)
        ));
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"invalid-prepare-stays-disabled"));
        poll(&net, 1006);
        assert!(packets(&b).is_empty());
        let enable = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: true,
                ipv6: true,
            }),
        )
        .unwrap();
        enable.activate().unwrap();
        enable.confirm().unwrap();
        a.wire
            .lock()
            .unwrap()
            .0
            .push_back(datagram(9, 0, &[], b"enabled-again"));
        poll(&net, 1006);
        assert_eq!(packets(&b).len(), 1);
    }
}

// IPv4 用例不能覆盖 IPv6 的不分片/扩展头作用域、ND 恢复与跨 zone 禁止；沿同一实际双 NIC、FIB 和配置租约验收。
mod ipv6_forwarding {
    use super::*;
    use core::net::Ipv6Addr;
    use packet_wire::{
        ethernet::EthernetHeader,
        icmpv6::{Icmpv6Packet, Message},
        ipv6::{Ipv6Header, Ipv6Packet},
        ndisc::{NdMessage, NdPacket, options::OptionValue},
    };
    const OUR_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const A: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 40, 0, 0, 0, 0, 15);
    const B: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 41, 0, 0, 0, 0, 15);
    const FROM: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 40, 0, 0, 0, 0, 9);
    const TO: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 41, 0, 0, 0, 0, 9);
    const FROM_MAC: [u8; 6] = [2, 0, 0, 0, 0, 9];
    const TO_MAC: [u8; 6] = [2, 0, 0, 0, 0, 10];
    fn frame(
        source: Ipv6Addr,
        destination: Ipv6Addr,
        hop: u8,
        protocol: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = vec![0; 54 + payload.len()];
        EthernetHeader {
            source: FROM_MAC,
            destination: OUR_MAC,
            type_or_length: 0x86dd,
            tags: &[],
        }
        .emit(&mut bytes)
        .unwrap();
        bytes[54..].copy_from_slice(payload);
        Ipv6Header {
            source,
            destination,
            hop_limit: hop,
            next_header: protocol,
            traffic_class: 0x2e,
            flow_label: 0xabcde,
        }
        .emit(&mut bytes[14..])
        .unwrap();
        bytes
    }
    fn neighbor(
        message: NdMessage,
        source: Ipv6Addr,
        destination: Ipv6Addr,
        mac: [u8; 6],
    ) -> Vec<u8> {
        let mut bytes = frame(source, destination, 255, 58, &[0; 32]);
        let target_mac = if destination.is_multicast() {
            let ip = destination.octets();
            [0x33, 0x33, ip[12], ip[13], ip[14], ip[15]]
        } else {
            OUR_MAC
        };
        EthernetHeader {
            source: mac,
            destination: target_mac,
            type_or_length: 0x86dd,
            tags: &[],
        }
        .emit(&mut bytes)
        .unwrap();
        OptionValue::LinkLayerAddress {
            source: matches!(message, NdMessage::NeighborSolicitation { .. }),
            address: &mac,
        }
        .emit(&mut bytes[78..])
        .unwrap();
        message.emit(&mut bytes[54..], source, destination).unwrap();
        bytes
    }
    fn ip(frame: &[u8]) -> Ipv6Packet<'_> {
        Ipv6Packet::parse(&frame[14..]).unwrap()
    }
    fn frames(device: &WireDevice) -> Vec<Vec<u8>> {
        core::mem::take(&mut device.wire.lock().unwrap().1)
    }
    fn inject(device: &WireDevice, bytes: Vec<u8>) {
        device.wire.lock().unwrap().0.push_back(bytes);
    }
    fn poll(net: &crate::namespace::Namespace, ms: i64) -> bool {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms))
            .backlog
    }
    fn data(frames: &[Vec<u8>]) -> Vec<&[u8]> {
        frames
            .iter()
            .filter(|frame| u16::from_be_bytes([frame[12], frame[13]]) == 0x86dd)
            .filter(|frame| {
                match packet_wire::ipv6::Ipv6Datagram::parse(&frame[14..])
                    .unwrap()
                    .upper_prefix()
                {
                    Ok(Some((protocol, _))) => protocol != 58,
                    _ => true,
                }
            })
            .map(Vec::as_slice)
            .collect()
    }
    fn error(bytes: &[Vec<u8>], kind: u8, code: u8, parameter: u32, quoted: &[u8]) {
        let bytes = bytes
            .iter()
            .find(|bytes| ip(bytes).upper_protocol() == 58 && ip(bytes).destination() == FROM)
            .unwrap();
        let outer = ip(bytes);
        assert_eq!(outer.source(), A);
        let icmp = Icmpv6Packet::parse(
            outer.upper_layer().unwrap(),
            outer.source(),
            outer.destination(),
        )
        .unwrap();
        assert_eq!((icmp.message_type(), icmp.code()), (kind, code));
        let Message::Error {
            parameter: actual,
            body,
        } = icmp.message()
        else {
            panic!("expected ICMPv6 error");
        };
        assert_eq!(actual, parameter);
        assert_eq!(body.quoted_bytes().unwrap(), quoted);
    }

    mod policy {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/forwarding_tests/policy.rs"
        ));
    }

    mod connection_state {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/forwarding_tests/connection_state.rs"
        ));
    }

    #[test]
    fn real_ipv6_transit_preserves_opaque_headers_and_scopes_and_resumes_without_reducing_hop_twice()
     {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, a, b) = routing_domain(89405);
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
        let configured = prepare_configuration(&net, plans.into_iter(), None).unwrap();
        configured.activate().unwrap();
        configured.confirm().unwrap();
        for now in [0, 1000, 2000, 2001] {
            poll(&net, now);
        }
        frames(&a);
        frames(&b);
        // 用实际 NS 让反向错误路径学到发送方；NA 的 Router 位来自当前转发配置。
        inject(
            &a,
            neighbor(
                NdMessage::NeighborSolicitation { target: A },
                FROM,
                A,
                FROM_MAC,
            ),
        );
        poll(&net, 2002);
        let replies = frames(&a);
        assert!(replies.iter().any(|bytes| matches!(NdPacket::parse(Icmpv6Packet::parse(ip(bytes).upper_layer().unwrap(), ip(bytes).source(), ip(bytes).destination()).unwrap()).unwrap().message(),
            NdMessage::NeighborAdvertisement { flags, target: A } if flags & 0x8000_0000 != 0)));
        // 未知出口邻居只生成 NS；真实 NA 唤醒原队列，同一原包只减一次 Hop Limit。
        let original = frame(FROM, TO, 64, 253, b"native-ipv6-cross-interface");
        inject(&a, original.clone());
        assert!(poll(&net, 2003));
        poll(&net, 2003);
        let requests = frames(&b);
        assert!(data(&requests).is_empty());
        let solicitation = requests
            .iter()
            .map(|bytes| ip(bytes))
            .find(|ip| {
                ip.upper_protocol() == 58
                    && matches!(
                        NdPacket::parse(
                            Icmpv6Packet::parse(
                                ip.upper_layer().unwrap(),
                                ip.source(),
                                ip.destination()
                            )
                            .unwrap()
                        )
                        .unwrap()
                        .message(),
                        NdMessage::NeighborSolicitation { target: TO }
                    )
            })
            .unwrap();
        inject(
            &b,
            neighbor(
                NdMessage::NeighborAdvertisement {
                    flags: 0x6000_0000,
                    target: TO,
                },
                TO,
                solicitation.source(),
                TO_MAC,
            ),
        );
        poll(&net, 2004);
        let wire = frames(&b);
        let sent = data(&wire);
        assert_eq!(
            sent.len(),
            1,
            "wire: {:?}, neighbor: {:?}",
            wire.iter()
                .map(|bytes| (bytes.len(), &bytes[..bytes.len().min(14)]))
                .collect::<Vec<_>>(),
            crate::inet::neighbor_snapshot(&net, 3).unwrap().get(0)
        );
        let mut expected = original[14..].to_vec();
        expected[7] = 63;
        assert_eq!(ip(sent[0]).bytes(), expected);
        assert_eq!(&sent[0][..6], &TO_MAC);
        frames(&a);
        // Destination Options 的 discard 动作仅由终点处理；同样的逐跳选项则在本路由器返回参数问题。
        let options = [253, 0, 0x80, 0, 0, 0, 0, 0];
        let opaque = frame(FROM, TO, 9, 60, &options);
        inject(&a, opaque.clone());
        poll(&net, 2005);
        let out = frames(&b);
        let out = data(&out);
        assert_eq!(out.len(), 1);
        assert_eq!(ip(out[0]).payload(), &options);
        assert_eq!(ip(out[0]).hop_limit(), 8);
        let fragment = [253, 0, 0, 8, 0x12, 0x34, 0x56, 0x78, 1, 2, 3, 4, 5, 6, 7, 8];
        inject(&a, frame(FROM, TO, 9, 44, &fragment));
        poll(&net, 2005);
        let out = frames(&b);
        let out = data(&out);
        assert_eq!(out.len(), 1);
        assert_eq!(ip(out[0]).payload(), &fragment);
        // 终点扩展头的 framing 本身也不由中间路由器解释；只改固定头构造独立的坏终点头输入。
        let mut malformed = frame(FROM, TO, 9, 253, b"opaque!!");
        malformed[20] = 60;
        inject(&a, malformed.clone());
        poll(&net, 2005);
        let out = frames(&b);
        let out = data(&out);
        assert_eq!(out.len(), 1);
        let forwarded = packet_wire::ipv6::Ipv6Datagram::parse(&out[0][14..]).unwrap();
        assert_eq!(forwarded.payload(), &malformed[54..]);
        assert_eq!(forwarded.hop_limit(), 8);
        let hop = frame(FROM, TO, 9, 0, &options);
        inject(&a, hop.clone());
        poll(&net, 2006);
        error(&frames(&a), 4, 2, 42, &hop[14..]);
        assert!(data(&frames(&b)).is_empty());
        let expired = frame(FROM, TO, 1, 253, b"hop-limit");
        inject(&a, expired.clone());
        poll(&net, 2007);
        error(&frames(&a), 3, 0, 0, &expired[14..]);
        configure_mtu(&net, 3, 1280).unwrap();
        let large = frame(FROM, TO, 8, 253, &[0x5a; 1300]);
        inject(&a, large.clone());
        poll(&net, 2008);
        error(&frames(&a), 2, 0, 1280, &large[14..14 + 1232]);
        assert!(data(&frames(&b)).is_empty());
        let absent = frame(FROM, "2001:db8:99::9".parse().unwrap(), 9, 253, b"no-route");
        inject(&a, absent.clone());
        poll(&net, 2009);
        error(&frames(&a), 1, 0, 0, &absent[14..]);
        // 对 ICMP 错误不再产生错误，即使其 Hop Limit 已到期。
        let mut nested = frame(FROM, TO, 1, 58, &[0; 8]);
        packet_wire::icmpv6::emit(&mut nested[54..], 1, 0, FROM, TO).unwrap();
        inject(&a, nested);
        poll(&net, 2010);
        assert!(frames(&a).is_empty());
        let scoped: Ipv6Addr = "fe80::9".parse().unwrap();
        inject(
            &a,
            neighbor(
                NdMessage::NeighborSolicitation { target: A },
                scoped,
                A,
                FROM_MAC,
            ),
        );
        poll(&net, 2011);
        frames(&a);
        let beyond = frame(scoped, TO, 9, 253, b"link-scope-must-not-cross-interface");
        inject(&a, beyond.clone());
        poll(&net, 2012);
        assert!(data(&frames(&b)).is_empty());
        let errors = frames(&a);
        let outer = errors
            .iter()
            .map(|bytes| ip(bytes))
            .find(|ip| ip.destination() == scoped)
            .unwrap();
        assert!(outer.source().is_unicast_link_local());
        let icmp = Icmpv6Packet::parse(
            outer.upper_layer().unwrap(),
            outer.source(),
            outer.destination(),
        )
        .unwrap();
        assert_eq!((icmp.message_type(), icmp.code()), (1, 2));
        // 激活关闭保留回滚所需的未发包；确认关闭才退休该族，另一族保持开启。
        b.transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        inject(&a, frame(FROM, TO, 7, 253, b"backpressure-and-rollback"));
        poll(&net, 2013);
        let paused = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: true,
                ipv6: false,
            }),
        )
        .unwrap();
        paused.activate().unwrap();
        b.transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 2014);
        assert!(data(&frames(&b)).is_empty());
        paused.abort().unwrap();
        poll(&net, 2015);
        let restored = frames(&b);
        let restored = data(&restored);
        assert_eq!(restored.len(), 1);
        assert_eq!(ip(restored[0]).hop_limit(), 6);
        b.transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        inject(&a, frame(FROM, TO, 8, 253, b"confirmed-disable-retires"));
        poll(&net, 2016);
        let off = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: true,
                ipv6: false,
            }),
        )
        .unwrap();
        off.activate().unwrap();
        off.confirm().unwrap();
        assert!(stack(&net).unwrap().lock().unwrap().forwarding_enabled.ipv4);
        b.transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        inject(
            &a,
            neighbor(
                NdMessage::NeighborSolicitation { target: A },
                FROM,
                A,
                FROM_MAC,
            ),
        );
        poll(&net, 2017);
        assert!(data(&frames(&b)).is_empty());
        let replies = frames(&a);
        assert!(replies.iter().any(|bytes| matches!(NdPacket::parse(Icmpv6Packet::parse(ip(bytes).upper_layer().unwrap(), ip(bytes).source(), ip(bytes).destination()).unwrap()).unwrap().message(),
            NdMessage::NeighborAdvertisement { flags, target: A } if flags & 0x8000_0000 == 0)));
        let on = prepare_configuration(
            &net,
            core::iter::empty(),
            Some(network_protocol::Forwarding {
                ipv4: true,
                ipv6: true,
            }),
        )
        .unwrap();
        on.activate().unwrap();
        on.confirm().unwrap();
        poll(&net, 2018);
        assert!(data(&frames(&b)).is_empty());
        inject(&a, original);
        poll(&net, 2019);
        assert_eq!(data(&frames(&b)).len(), 1);
    }
}
