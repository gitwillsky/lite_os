// 新增控制协议的时序/背压不能由 IPv4 socket 用例或静态审查覆盖；复用实际 NET 和 NIC loan 夹具。
mod ipv6_link {
    use super::*;
    use core::net::Ipv6Addr;
    use network_protocol::{AddressState as State, InterfaceAddress};
    use packet_wire::{
        ethernet::{EthernetFrame, EthernetHeader},
        icmpv6::Icmpv6Packet,
        ipv6::{
            Ipv6Header, Ipv6Packet,
            options::{self, OptionData},
        },
        mld::{self, MldMessage},
        multicast::{RecordType, SourceQueryHeader},
        ndisc::{NdMessage, NdPacket, options::OptionValue},
    };
    const LOCAL: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0xff, 0xfe00, 1);
    const GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 1, 0xff00, 1);
    const ALL: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
    const PEER: Ipv6Addr = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
    const MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
    fn domain() -> (Arc<crate::namespace::Namespace>, Arc<WireDevice>) {
        let net = crate::namespace::Namespace::create().unwrap();
        let device = Arc::new(WireDevice::new());
        let nic: Arc<dyn device_model::network::NetworkDevice> = device.clone();
        net.install_protocol(prepare_network(&net, core::slice::from_ref(&nic), true).unwrap());
        configure_up(&net, 2, true).unwrap();
        (net, device)
    }
    fn poll(net: &crate::namespace::Namespace, ms: i64) {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    }
    fn state(net: &crate::namespace::Namespace) -> Option<InterfaceAddress> {
        let rows = address_snapshot(net, 2).unwrap();
        (0..rows.len()).filter_map(|index| rows.get(index)).find(|row| row.address.is_ipv6())
    }
    fn frames(device: &WireDevice) -> Vec<Vec<u8>> {
        core::mem::take(&mut device.wire.lock().unwrap().1)
    }
    fn enqueue(device: &WireDevice, frame: Vec<u8>) {
        device.wire.lock().unwrap().0.push_back(frame);
    }
    fn parse(frame: &[u8]) -> (Ipv6Packet<'_>, Icmpv6Packet<'_>) {
        let ethernet = EthernetFrame::parse(frame).unwrap();
        assert_eq!(ethernet.type_or_length(), 0x86dd);
        let ip = Ipv6Packet::parse(ethernet.payload()).unwrap();
        let icmp =
            Icmpv6Packet::parse(ip.upper_layer().unwrap(), ip.source(), ip.destination()).unwrap();
        (ip, icmp)
    }
    fn envelope(frame: &mut [u8], source: Ipv6Addr, destination: Ipv6Addr, mld: bool, hops: u8) {
        if mld {
            options::emit(
                &mut frame[54..62],
                58,
                &[OptionData {
                    kind: 5,
                    data: &[0, 0],
                    alignment: (2, 0),
                }],
            )
            .unwrap();
        }
        Ipv6Header {
            source,
            destination,
            next_header: if mld { 0 } else { 58 },
            traffic_class: 0,
            flow_label: 0,
            hop_limit: hops,
        }
        .emit(&mut frame[14..])
        .unwrap();
        let destination_mac = if destination.is_multicast() {
            let bytes = destination.octets();
            [0x33, 0x33, bytes[12], bytes[13], bytes[14], bytes[15]]
        } else {
            [2, 0, 0, 0, 0, 1]
        };
        EthernetHeader {
            source: MAC,
            destination: destination_mac,
            type_or_length: 0x86dd,
            tags: &[],
        }
        .emit(frame)
        .unwrap();
    }
    fn solicitation(source: Ipv6Addr, hops: u8) -> Vec<u8> {
        let mut frame = vec![0; 86];
        OptionValue::LinkLayerAddress {
            source: true,
            address: &MAC,
        }
        .emit(&mut frame[78..])
        .unwrap();
        NdMessage::NeighborSolicitation { target: LOCAL }
            .emit(&mut frame[54..], source, GROUP)
            .unwrap();
        envelope(&mut frame, source, GROUP, false, hops);
        frame
    }
    fn query(group: Ipv6Addr, sources: &[Ipv6Addr], v1: bool) -> Vec<u8> {
        let mut frame = vec![0; 62 + if v1 { 24 } else { 28 + sources.len() * 16 }];
        if !v1 {
            for (bytes, source) in frame[90..].as_chunks_mut::<16>().0.iter_mut().zip(sources) {
                bytes.copy_from_slice(&source.octets());
            }
        }
        mld::emit_query(
            &mut frame[62..],
            0,
            group,
            (!v1).then_some(SourceQueryHeader {
                flags: 0,
                suppress: false,
                robustness: 2,
                query_interval_code: 125,
                source_count: sources.len() as u16,
            }),
            PEER,
            ALL,
        )
        .unwrap();
        envelope(&mut frame, PEER, ALL, true, 1);
        frame
    }
    fn echo(source: Ipv6Addr, destination: Ipv6Addr, data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; 62 + data.len()];
        frame[58..62].copy_from_slice(&[0x93, 0xab, 0xff, 0xff]);
        frame[62..].copy_from_slice(data);
        packet_wire::icmpv6::emit(&mut frame[54..], 128, 0, source, destination).unwrap();
        envelope(&mut frame, source, destination, false, 7);
        frame
    }
    fn learn_peer(net: &crate::namespace::Namespace, device: &WireDevice) {
        let mut solicitation = solicitation(PEER, 255);
        // ND 选项才是依据；不能凭收到帧的 L2 source 覆盖它。
        solicitation[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 99]);
        enqueue(device, solicitation);
        poll(net, 2001);
        let output = frames(device);
        assert_eq!(output.len(), 1);
        assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
    }
    fn ready(net: &crate::namespace::Namespace, device: &WireDevice) {
        for ms in [0, 1000, 2000, 2001] {
            poll(net, ms);
        }
        assert_eq!(state(net).unwrap().state, State::Preferred);
        frames(device);
    }
    // 固定轻量接口行无法覆盖多地址枚举；验证真实地址 owner、回环、过滤与冻结后退出。
    #[test]
    fn address_query_includes_real_loopback_families_and_freezes_interface_lifetime() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        ready(&net, &device);
        let all = address_snapshot(&net, 0).unwrap();
        let rows = (0..all.len()).filter_map(|index| all.get(index)).collect::<Vec<_>>();
        assert!(rows.iter().any(|row| row.interface_index == 1 && row.address == core::net::IpAddr::V4(Ipv4Addr::LOCALHOST) && row.prefix_length == 8));
        assert!(rows.iter().any(|row| row.interface_index == 1 && row.address == core::net::IpAddr::V6(Ipv6Addr::LOCALHOST) && row.prefix_length == 128));
        assert!(rows.iter().any(|row| row.interface_index == 2 && row.address == core::net::IpAddr::V6(LOCAL) && row.state == State::Preferred));
        let selected = address_snapshot(&net, 2).unwrap();
        assert_eq!(selected.len(), 1);
        assert!(matches!(address_snapshot(&net, 999), Err(SocketError::NoDevice)));
        configure_up(&net, 2, false).unwrap();
        assert!(address_snapshot(&net, 2).unwrap().is_empty());
        assert_eq!(selected.get(0).unwrap().state, State::Preferred);
        configure_up(&net, 1, false).unwrap();
        let down = address_snapshot(&net, 0).unwrap();
        assert_eq!(down.len(), 1);
        assert_eq!(down.get(0).unwrap().address, core::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    // 固定 wire + 生产 UDP/端口/NET/ND；IPv4 回归无法证明双族 wildcard、zone 与 IPv6 checksum。
    #[test]
    fn udp6_loopback_dual_stack_and_v6only_use_the_same_port_and_payload_owners() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let net = crate::namespace::Namespace::create().unwrap();
        net.install_protocol(prepare_network(&net, &[], true).unwrap());
        let server = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        let any: InetAddress = "[::]:43300".parse().unwrap();
        server.bind(any).unwrap();
        assert_eq!(server.option(InternetOption::Ipv6Only), Ok(0));
        assert_eq!(
            server.set_ipv6_option(InternetOption::Ipv6Only, 1),
            Err(SocketError::Invalid)
        );
        let v4 = scoped_endpoint(&net, InetProtocol::Udp);
        assert_eq!(v4.bind(address(43300)), Err(SocketError::AddressInUse));
        let target: InetAddress = "127.0.0.1:43300".parse().unwrap();
        v4.send_to(b"mapped", Some(target), &[]).unwrap();
        advance_actual_domains();
        let (bytes, source) = udp_read(&server, false).unwrap();
        assert_eq!(bytes, b"mapped");
        let Some(InetAddress::V6(source)) = source else {
            panic!("IPv6 socket must report mapped sender");
        };
        assert_eq!(source.ip().to_ipv4_mapped(), Some(Ipv4Addr::LOCALHOST));
        server
            .send_to(b"reply", Some(InetAddress::V6(source)), &[])
            .unwrap();
        advance_actual_domains();
        assert_eq!(udp_read(&v4, false).unwrap().0, b"reply");
        let client = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        client.connect("[::]:43300".parse().unwrap()).unwrap();
        assert_eq!(
            client.peer_address().unwrap(),
            "[::1]:43300".parse().unwrap()
        );
        client.send_to(b"native", None, &[]).unwrap();
        advance_actual_domains();
        let (bytes, source) = udp_read(&server, false).unwrap();
        assert_eq!(bytes, b"native");
        assert_eq!(
            source.unwrap().ip(),
            core::net::IpAddr::V6(Ipv6Addr::LOCALHOST)
        );
        let only = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        only.set_ipv6_option(InternetOption::Ipv6Only, 1).unwrap();
        assert_eq!(
            only.bind("[::ffff:127.0.0.1]:43301".parse().unwrap()),
            Err(SocketError::Invalid)
        );
        only.bind("[::]:43301".parse().unwrap()).unwrap();
        let independent = scoped_endpoint(&net, InetProtocol::Udp);
        independent
            .bind("127.0.0.1:43301".parse().unwrap())
            .unwrap();
        assert_eq!(
            only.send_to(b"no", Some(target), &[]),
            Err(SocketError::AddressFamilyNotSupported)
        );
        for endpoint in [server, v4, client, only, independent] {
            endpoint.close_description();
        }
        let network = stack(&net).unwrap().lock().unwrap();
        assert!(network.udp_endpoints.is_empty());
        assert_eq!(network.sockets.iter().count(), 0);
    }
    #[test]
    fn udp6_link_scope_checksum_neighbor_and_frozen_hop_limit_are_actual_wire_facts() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        ready(&net, &device);
        let endpoint = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        let local = InetAddress::V6(core::net::SocketAddrV6::new(LOCAL, 43302, 0, 2));
        assert_eq!(
            endpoint.bind(InetAddress::new(LOCAL.into(), 43302)),
            Err(SocketError::Invalid)
        );
        assert_eq!(
            endpoint.bind(InetAddress::V6(core::net::SocketAddrV6::new(
                LOCAL, 43302, 0, 99
            ))),
            Err(SocketError::NoDevice)
        );
        endpoint.bind(local).unwrap();
        assert_eq!(endpoint.address().unwrap(), local);
        assert_eq!(endpoint.option(InternetOption::Ipv6Only), Ok(1));
        assert_eq!(endpoint.option(InternetOption::BoundToDevice), Ok(2));
        let peer = InetAddress::V6(core::net::SocketAddrV6::new(PEER, 44000, 0, 2));
        assert_eq!(
            endpoint.connect(InetAddress::V6(core::net::SocketAddrV6::new(
                PEER, 44000, 0, 1
            ))),
            Err(SocketError::Invalid)
        );
        endpoint.connect(peer).unwrap();
        assert_eq!(endpoint.peer_address().unwrap(), peer);
        let mut incoming = vec![0xa5; 14 + 40 + 8 + 3];
        packet_wire::ethernet::EthernetHeader {
            destination: [2, 0, 0, 0, 0, 1],
            source: MAC,
            type_or_length: 0x86dd,
            tags: &[],
        }
        .emit(&mut incoming)
        .unwrap();
        Ipv6Header {
            source: PEER,
            destination: LOCAL,
            traffic_class: 0,
            flow_label: 0,
            next_header: 17,
            hop_limit: 0,
        }
        .emit(&mut incoming[14..])
        .unwrap();
        packet_wire::udp::UdpHeader {
            source: PEER.into(),
            destination: LOCAL.into(),
            source_port: 44000,
            destination_port: 43302,
        }
        .emit_prefix(&mut incoming[54..62], b"udp", true)
        .unwrap();
        incoming[62..].copy_from_slice(b"udp");
        let mut invalid = incoming.clone();
        invalid[60..62].fill(0);
        enqueue(&device, invalid);
        enqueue(&device, incoming);
        poll(&net, 2002);
        assert_eq!(
            udp_read(&endpoint, false).unwrap(),
            (b"udp".to_vec(), Some(peer))
        );
        assert_eq!(udp_read(&endpoint, false), Err(SocketError::Again));
        learn_peer(&net, &device);
        endpoint
            .set_ipv6_option(InternetOption::Ipv6HopLimit, 0)
            .unwrap();
        endpoint.set_hop_limit(Some(33)).unwrap();
        endpoint.send_to(b"out", None, &[]).unwrap();
        endpoint
            .set_ipv6_option(InternetOption::Ipv6HopLimit, 17)
            .unwrap();
        poll(&net, 2003);
        let output = frames(&device);
        let frame = output
            .iter()
            .find(|frame| Ipv6Packet::parse(&frame[14..]).is_ok_and(|ip| ip.upper_protocol() == 17))
            .unwrap();
        let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
        assert_eq!(ip.hop_limit(), 0);
        assert_eq!(ip.source(), LOCAL);
        assert_eq!(ip.destination(), PEER);
        let packet = packet_wire::udp::UdpPacket::parse(
            ip.upper_layer().unwrap(),
            LOCAL.into(),
            PEER.into(),
        )
        .unwrap();
        assert_eq!(packet.payload(), b"out");
        assert_eq!(packet.source_port(), 43302);
        assert_eq!(EthernetFrame::parse(frame).unwrap().destination(), MAC);
        // 探测失败必须释放原 UDP 队列，不能在同一轮重建 INCOMPLETE 而永远挡住后续数据报。
        let absent: Ipv6Addr = "fe80::9".parse().unwrap();
        endpoint
            .connect(InetAddress::V6(core::net::SocketAddrV6::new(
                absent, 44000, 0, 2,
            )))
            .unwrap();
        for index in 0..8 {
            if index % 2 == 0 {
                endpoint.send_to(&[], None, &[]).unwrap();
            } else {
                endpoint.send_to(b"survive", Some(peer), &[]).unwrap();
            }
        }
        assert_eq!(endpoint.send_to(&[], None, &[]), Err(SocketError::Again));
        for ms in [2004, 2004, 3004, 4004, 5004] {
            poll(&net, ms);
        }
        assert!(endpoint.poll_state().writable);
        let output = frames(&device);
        let solicitations = output.iter().filter(|frame| {
            let Ok(ip) = Ipv6Packet::parse(&frame[14..]) else { return false; };
            if ip.upper_protocol() != 58 { return false; }
            let Ok(icmp) = Icmpv6Packet::parse(ip.upper_layer().unwrap(), ip.source(), ip.destination()) else { return false; };
            packet_wire::ndisc::NdPacket::parse(icmp).is_ok_and(|nd| matches!(nd.message(), NdMessage::NeighborSolicitation { target } if target == absent))
        }).count();
        assert_eq!(solicitations, 3);
        let delivered = output
            .iter()
            .filter(|frame| {
                let ip = Ipv6Packet::parse(&frame[14..]).unwrap();
                ip.upper_protocol() == 17
                    && packet_wire::udp::UdpPacket::parse(
                        ip.upper_layer().unwrap(),
                        ip.source().into(),
                        ip.destination().into(),
                    )
                    .is_ok_and(|udp| ip.destination() == PEER && udp.payload() == b"survive")
            })
            .count();
        assert_eq!(
            delivered, 4,
            "failure must preserve other destinations and their payloads"
        );
        endpoint.close_description();
        assert!(
            stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .udp_endpoints
                .is_empty()
        );
    }
    #[test]
    fn dad_reports_then_probes_and_only_assigns_after_successful_transmission_and_wait() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        device
            .transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 0);
        assert_eq!(
            state(&net),
            Some(InterfaceAddress {
                interface_index: 2,
                name: interface_snapshot(&net, 2).unwrap().name,
                address: LOCAL.into(),
                prefix_length: 64,
                state: State::Tentative,
                usable: false,
                origin: network_protocol::AddressOrigin::Interface,
                on_link: true,
                broadcast: None,
                preferred_lifetime: u32::MAX,
                valid_lifetime: u32::MAX,
            })
        );
        poll(&net, 1000);
        assert!(frames(&device).is_empty());
        assert!(
            !stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .interface(2)
                .unwrap()
                .ipv6
                .as_ref()
                .unwrap()
                .due(1000),
            "capacity wait must not spin timer"
        );
        poll(&net, 10000);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        device
            .transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 10001);
        let output = frames(&device);
        assert_eq!(output.len(), 2);
        let (ip, report) = parse(&output[0]);
        assert_eq!((ip.source(), ip.hop_limit()), (Ipv6Addr::UNSPECIFIED, 1));
        assert_eq!(
            ip.extensions()
                .next()
                .unwrap()
                .options()
                .unwrap()
                .find(|option| option.kind == 5)
                .unwrap()
                .data,
            &[0, 0]
        );
        let MldMessage::ReportV2 { mut records, .. } = MldMessage::parse(report).unwrap() else {
            panic!("v2 report required");
        };
        let record = records.next().unwrap();
        assert_eq!(
            (record.record_type(), record.group()),
            (RecordType::ChangeToExclude as u8, GROUP.into())
        );
        let (ip, probe) = parse(&output[1]);
        assert_eq!(
            (ip.source(), ip.destination(), ip.hop_limit()),
            (Ipv6Addr::UNSPECIFIED, GROUP, 255)
        );
        let probe = NdPacket::parse(probe).unwrap();
        assert!(
            probe
                .options()
                .all(|option| option.kind() == 14 && option.bytes().len() == 8)
        );
        poll(&net, 11000);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        poll(&net, 11001);
        assert_eq!(state(&net).unwrap().state, State::Preferred);
        frames(&device);
        poll(&net, 11002);
        assert!(frames(&device).iter().any(|frame| {
            let (ip, icmp) = parse(frame);
            ip.source() == LOCAL && icmp.message_type() == 143
        }));
        configure_up(&net, 2, false).unwrap();
        assert!(state(&net).is_none());
        configure_up(&net, 2, true).unwrap();
        poll(&net, 12000);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        device.set_carrier(false);
        poll(&net, 12001);
        assert!(state(&net).is_none());
        device.set_carrier(true);
        poll(&net, 12002);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        configure_mtu(&net, 2, 1279).unwrap();
        assert!(state(&net).is_none());
    }
    #[test]
    fn reflected_probe_extends_dad_but_remote_collision_with_same_mac_is_duplicate() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        poll(&net, 0);
        poll(&net, 1000);
        let probe = frames(&device)
            .into_iter()
            .find(|frame| parse(frame).1.message_type() == 135)
            .unwrap();
        enqueue(&device, probe);
        poll(&net, 1001);
        for ms in [2000, 3000, 4000] {
            poll(&net, ms);
            assert_eq!(state(&net).unwrap().state, State::Tentative);
        }
        poll(&net, 5000);
        assert_eq!(state(&net).unwrap().state, State::Preferred);
        configure_up(&net, 2, false).unwrap();
        configure_up(&net, 2, true).unwrap();
        poll(&net, 6000);
        poll(&net, 7000);
        let mut probe = frames(&device)
            .into_iter()
            .rev()
            .find(|frame| parse(frame).1.message_type() == 135)
            .unwrap();
        probe[80] ^= 1;
        packet_wire::icmpv6::emit(&mut probe[54..], 135, 0, Ipv6Addr::UNSPECIFIED, GROUP).unwrap();
        enqueue(&device, probe);
        poll(&net, 7001);
        assert_eq!(state(&net).unwrap().state, State::Duplicate);
        poll(&net, 10000);
        assert_eq!(state(&net).unwrap().state, State::Duplicate);
        // timer 与已入队的 NA 同时到期，且冲突位于 RX 预算之后，不能先公布 Preferred。
        configure_up(&net, 2, false).unwrap();
        configure_up(&net, 2, true).unwrap();
        poll(&net, 11000);
        poll(&net, 12000);
        frames(&device);
        for _ in 0..NETWORK_RX_BUDGET {
            enqueue(&device, solicitation(PEER, 254));
        }
        let mut advertisement = vec![0; 86];
        OptionValue::LinkLayerAddress {
            source: false,
            address: &MAC,
        }
        .emit(&mut advertisement[78..])
        .unwrap();
        NdMessage::NeighborAdvertisement {
            target: LOCAL,
            flags: 0x2000_0000,
        }
        .emit(&mut advertisement[54..], PEER, ALL)
        .unwrap();
        envelope(&mut advertisement, PEER, ALL, false, 255);
        enqueue(&device, advertisement);
        poll(&net, 13000);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        poll(&net, 13000);
        assert_eq!(state(&net).unwrap().state, State::Duplicate);
    }
    #[test]
    fn tentative_ignores_address_resolution_and_preferred_answers_only_valid_nd() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        poll(&net, 0);
        poll(&net, 1000);
        frames(&device);
        enqueue(&device, solicitation(PEER, 255));
        poll(&net, 1001);
        assert_eq!(state(&net).unwrap().state, State::Tentative);
        assert!(
            !frames(&device)
                .iter()
                .any(|frame| parse(frame).1.message_type() == 136)
        );
        poll(&net, 2000);
        poll(&net, 2001);
        frames(&device);
        let valid = solicitation(PEER, 255);
        let mut bad = valid.clone();
        bad[56] ^= 1;
        enqueue(&device, bad);
        enqueue(&device, solicitation(PEER, 254));
        let mut fragmented = vec![0; valid.len() + 8];
        fragmented[..54].copy_from_slice(&valid[..54]);
        fragmented[20] = 44;
        fragmented[18..20].copy_from_slice(&40u16.to_be_bytes());
        fragmented[54] = 58;
        fragmented[62..].copy_from_slice(&valid[54..]);
        enqueue(&device, fragmented);
        poll(&net, 2002);
        assert!(frames(&device).is_empty());
        enqueue(&device, valid);
        poll(&net, 2003);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        let (ip, icmp) = parse(&output[0]);
        assert_eq!(
            (ip.source(), ip.destination(), ip.hop_limit()),
            (LOCAL, PEER, 255)
        );
        assert_eq!(EthernetFrame::parse(&output[0]).unwrap().destination(), MAC);
        let nd = NdPacket::parse(icmp).unwrap();
        assert!(matches!(
            nd.message(),
            NdMessage::NeighborAdvertisement {
                flags: 0xe000_0000,
                target: LOCAL
            }
        ));
        let option = nd.options().next().unwrap();
        assert_eq!(option.kind(), 2);
        assert_eq!(&option.bytes()[2..], &[2, 0, 0, 0, 0, 1]);
        let mut dad = vec![0; 78];
        NdMessage::NeighborSolicitation { target: LOCAL }
            .emit(&mut dad[54..], Ipv6Addr::UNSPECIFIED, GROUP)
            .unwrap();
        envelope(&mut dad, Ipv6Addr::UNSPECIFIED, GROUP, false, 255);
        enqueue(&device, dad);
        poll(&net, 2004);
        let output = frames(&device);
        let (ip, icmp) = parse(&output[0]);
        assert_eq!(ip.destination(), ALL);
        assert!(matches!(
            NdPacket::parse(icmp).unwrap().message(),
            NdMessage::NeighborAdvertisement {
                flags: 0xa000_0000,
                ..
            }
        ));
    }
    #[test]
    fn mld_query_merges_bounded_sources_splits_reports_and_interoperates_with_v1() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        ready(&net, &device);
        let sources: Vec<Ipv6Addr> = (1..=128)
            .map(|last| Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last))
            .collect();
        enqueue(&device, query(GROUP, &sources[..89], false));
        enqueue(&device, query(GROUP, &sources[89..], false));
        enqueue(
            &device,
            query(GROUP, &["2001:db8::ffff".parse().unwrap()], false),
        );
        poll(&net, 2002);
        device
            .transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 2003);
        assert!(frames(&device).is_empty());
        device
            .transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 2004);
        let mut answered = Vec::new();
        for frame in frames(&device) {
            let (ip, icmp) = parse(&frame);
            assert!(ip.bytes().len() <= 1500);
            let MldMessage::ReportV2 { records, .. } = MldMessage::parse(icmp).unwrap() else {
                panic!("v2 report");
            };
            for record in records {
                assert_eq!(record.record_type(), RecordType::ModeIsInclude as u8);
                answered.extend(record.sources());
            }
        }
        assert_eq!(
            answered,
            sources
                .iter()
                .copied()
                .map(core::net::IpAddr::V6)
                .collect::<Vec<_>>()
        );
        enqueue(&device, query(Ipv6Addr::UNSPECIFIED, &[], true));
        poll(&net, 2005);
        poll(&net, 2006);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        let (ip, icmp) = parse(&output[0]);
        assert_eq!(ip.destination(), GROUP);
        assert!(matches!(
            MldMessage::parse(icmp).unwrap(),
            MldMessage::ReportV1 {
                done: false,
                group: GROUP
            }
        ));
        poll(&net, 252005);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_eq!(parse(&output[0]).1.message_type(), 143);
    }

    // DAD/ND 用例不覆盖借用的大载荷回复、控制协议 Hop Limit 与一般主机报文的区别。
    #[test]
    fn echo_returns_exact_data_and_addresses_without_retaining_rx_or_copying_padding() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        ready(&net, &device);
        learn_peer(&net, &device);
        for (index, size) in [0, 1, 1452].into_iter().enumerate() {
            let data: Vec<_> = (0..size).map(|byte| (byte % 251) as u8).collect();
            let mut request = echo(PEER, LOCAL, &data);
            // IPv6 TC 不改变伪头 checksum；Ethernet padding 不属于 Echo 数据。
            request[14] = 0x62;
            request[15] = 0xe0;
            if size == 0 {
                request.extend_from_slice(&[0x5a; 18]);
            }
            enqueue(&device, request);
            poll(&net, 2002 + index as i64);
            let output = frames(&device);
            assert_eq!(output.len(), 1);
            let ethernet = EthernetFrame::parse(&output[0]).unwrap();
            assert_eq!(
                (ethernet.source(), ethernet.destination()),
                ([2, 0, 0, 0, 0, 1], MAC)
            );
            let (ip, icmp) = parse(&output[0]);
            assert_eq!(
                (
                    ip.source(),
                    ip.destination(),
                    ip.hop_limit(),
                    ip.traffic_class()
                ),
                (LOCAL, PEER, 64, 0x2e)
            );
            assert_eq!(
                icmp.message(),
                packet_wire::icmpv6::Message::Echo {
                    reply: true,
                    identifier: 0x93ab,
                    sequence: 0xffff,
                    data: &data
                }
            );
            assert_eq!(output[0].len(), 62 + data.len());
            let wire = device.wire.lock().unwrap();
            assert!(!wire.2.reserved && wire.2.received.iter().all(Option::is_none));
        }
        // all-nodes 和 solicited-node 都属于当前接口；组播回复使用该接口的可用单播源。
        for (index, destination) in [ALL, GROUP].into_iter().enumerate() {
            enqueue(&device, echo(PEER, destination, b"multicast"));
            poll(&net, 2005 + index as i64);
            let output = frames(&device);
            assert_eq!(output.len(), 1);
            let (ip, icmp) = parse(&output[0]);
            assert_eq!((ip.source(), ip.destination()), (LOCAL, PEER));
            assert_eq!(icmp.message_type(), 129);
        }
        // atomic fragment 是完整报文；它可以回应 Echo，却不能放松 ND 的禁分片约束。
        let request = echo(PEER, LOCAL, b"atomic-echo");
        let mut atomic = vec![0; request.len() + 8];
        atomic[..54].copy_from_slice(&request[..54]);
        atomic[20] = 44;
        atomic[54] = 58;
        let length = (atomic.len() - 54) as u16;
        atomic[18..20].copy_from_slice(&length.to_be_bytes());
        atomic[62..].copy_from_slice(&request[54..]);
        enqueue(&device, atomic);
        poll(&net, 2007);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_eq!(parse(&output[0]).1.body(), &request[58..]);
    }
    #[test]
    fn echo_suppresses_unusable_addresses_invalid_sources_and_malformed_requests() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        poll(&net, 0);
        enqueue(&device, echo(PEER, LOCAL, b"tentative"));
        poll(&net, 1);
        assert!(
            !frames(&device)
                .iter()
                .any(|frame| parse(frame).1.message_type() == 129)
        );
        for ms in [1000, 2000, 2001] {
            poll(&net, ms);
        }
        frames(&device);
        let request = echo(PEER, LOCAL, b"must-not-reflect");
        let mut bad_checksum = request.clone();
        bad_checksum[56] ^= 1;
        enqueue(&device, bad_checksum);
        for (kind, code) in [(128, 1), (129, 0), (1, 0), (200, 0)] {
            let mut invalid = request.clone();
            packet_wire::icmpv6::emit(&mut invalid[54..], kind, code, PEER, LOCAL).unwrap();
            enqueue(&device, invalid);
        }
        for source in [
            Ipv6Addr::UNSPECIFIED,
            ALL,
            "::ffff:192.0.2.1".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        ] {
            enqueue(&device, echo(source, LOCAL, b"no-return-route"));
        }
        let mut wrong_mac = request.clone();
        wrong_mac[5] = 99;
        enqueue(&device, wrong_mac);
        // 非本机单播由转发用例覆盖；线上回环目的不属于可转发地址。
        enqueue(&device, echo(PEER, Ipv6Addr::LOCALHOST, b"invalid-wire-destination"));
        let mut truncated = request.clone();
        truncated.truncate(60);
        enqueue(&device, truncated);
        // 未识别选项的 discard 动作不能被 Echo 接收旁路绕开。
        let mut unknown = vec![0; request.len() + 8];
        unknown[..54].copy_from_slice(&request[..54]);
        unknown[20] = 0;
        unknown[54] = 58;
        unknown[56] = 0x40;
        let length = (unknown.len() - 54) as u16;
        unknown[18..20].copy_from_slice(&length.to_be_bytes());
        unknown[62..].copy_from_slice(&request[54..]);
        enqueue(&device, unknown);
        poll(&net, 2002);
        assert!(frames(&device).is_empty());
        configure_up(&net, 2, false).unwrap();
        configure_up(&net, 2, true).unwrap();
        poll(&net, 3000);
        let mut conflict = vec![0; 78];
        NdMessage::NeighborSolicitation { target: LOCAL }
            .emit(&mut conflict[54..], Ipv6Addr::UNSPECIFIED, GROUP)
            .unwrap();
        envelope(&mut conflict, Ipv6Addr::UNSPECIFIED, GROUP, false, 255);
        enqueue(&device, conflict);
        enqueue(&device, request.clone());
        poll(&net, 3001);
        assert_eq!(state(&net).unwrap().state, State::Duplicate);
        assert!(
            !frames(&device)
                .iter()
                .any(|frame| parse(frame).1.message_type() == 129)
        );
        configure_up(&net, 2, false).unwrap();
        enqueue(&device, request);
        poll(&net, 3002);
        assert!(frames(&device).is_empty());
    }
    #[test]
    fn echo_tx_backpressure_releases_loan_counts_once_and_obeys_shared_receive_budget() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        ready(&net, &device);
        learn_peer(&net, &device);
        device
            .transmit_available
            .store(false, core::sync::atomic::Ordering::Relaxed);
        enqueue(&device, echo(PEER, LOCAL, b"full-tx-ring"));
        poll(&net, 2002);
        assert!(frames(&device).is_empty());
        let network = stack(&net).unwrap().lock().unwrap();
        assert_eq!(
            network
                .interface(2)
                .unwrap()
                .device
                .local_statistics()
                .transmitted_dropped,
            1
        );
        drop(network);
        assert!(
            device
                .wire
                .lock()
                .unwrap()
                .2
                .received
                .iter()
                .all(Option::is_none)
        );
        device
            .transmit_available
            .store(true, core::sync::atomic::Ordering::Relaxed);
        poll(&net, 2003);
        assert!(
            frames(&device).is_empty(),
            "unsent stateless reply does not retain an RX loan or build an unbounded queue"
        );
        for _ in 0..NETWORK_RX_BUDGET + 1 {
            enqueue(&device, echo(PEER, LOCAL, b"bounded-progress"));
        }
        let outcome = stack(&net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(2004));
        assert!(outcome.backlog);
        assert_eq!(frames(&device).len(), NETWORK_RX_BUDGET);
        poll(&net, 2004);
        let output = frames(&device);
        assert_eq!(output.len(), 1);
        assert_eq!(parse(&output[0]).1.message_type(), 129);
        let wire = device.wire.lock().unwrap();
        assert!(!wire.2.reserved && wire.2.received.iter().all(Option::is_none));
    }

    mod error_contract {
        use super::*;
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ipv6_error_tests.rs"
        ));
    }

    mod packet_info_contract {
        use super::*;
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ipv6_packet_info_tests.rs"
        ));
    }

    mod neighbor_contract {
        use super::*;
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/ipv6_neighbor_tests.rs"
        ));
    }

    mod configuration_contract {
        use super::*;
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ipv6_configuration_tests.rs"));
    }
    mod tcp_contract {
        use super::*;
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ipv6_tcp_tests.rs"));
    }
    mod raw_contract {
        use super::*;
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ipv6_raw_tests.rs"));
    }

}
