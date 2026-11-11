// IGMP 版本/报告抑制、跨层成员准入及查询来源宽度不能由 IPv6 单播或 codec 测试证明。
// 复用现有生产 NET/NIC，边界只产生标准报文，不复制成员状态机。
mod multicast_contract {
    use super::*;
    use core::net::IpAddr;
    use packet_wire::{
        ethernet::{EthernetFrame, EthernetHeader},
        igmp::{IgmpMessage, IgmpPacket},
        ipv4::{Ipv4Header, Ipv4Packet},
        multicast::{RecordType, SourceQueryHeader},
    };
    const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const PEER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
    const GROUP: Ipv4Addr = Ipv4Addr::new(239, 1, 1, 1);
    const ALL: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 1);
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
        configure_address(&net, 2, LOCAL).unwrap();
        configure_netmask(&net, 2, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
        configure_up(&net, 2, true).unwrap();
        for ms in [0, 1000, 2000, 2001] {
            poll(&net, ms);
        }
        device.wire.lock().unwrap().1.clear();
        (net, device)
    }
    fn poll(net: &crate::namespace::Namespace, ms: i64) {
        stack(net)
            .unwrap()
            .lock()
            .unwrap()
            .poll_at(Instant::from_millis(ms));
    }
    fn input(device: &WireDevice, packet: Vec<u8>) {
        device.wire.lock().unwrap().0.push_back(packet);
    }
    fn reports(device: &WireDevice) -> Vec<Vec<u8>> {
        core::mem::take(&mut device.wire.lock().unwrap().1)
            .into_iter()
            .filter(|frame| {
                let ethernet = EthernetFrame::parse(frame).unwrap();
                ethernet.type_or_length() == 0x0800
                    && Ipv4Packet::parse(ethernet.payload()).unwrap().protocol() == 2
            })
            .collect()
    }
    fn envelope(
        payload: &[u8],
        source: Ipv4Addr,
        destination: Ipv4Addr,
        alert: bool,
        ttl: u8,
    ) -> Vec<u8> {
        let header = if alert { 24 } else { 20 };
        let mut frame = vec![0; 14 + header + payload.len()];
        frame[14 + header..].copy_from_slice(payload);
        Ipv4Header {
            source,
            destination,
            protocol: 2,
            hop_limit: ttl,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0x4000,
            options: if alert { &[0x94, 4, 0, 0] } else { &[] },
        }
        .emit(&mut frame[14..])
        .unwrap();
        let address = destination.octets();
        EthernetHeader {
            source: [2, 0, 0, 0, 0, 2],
            destination: [1, 0, 0x5e, address[1] & 0x7f, address[2], address[3]],
            type_or_length: 0x0800,
            tags: &[],
        }
        .emit(&mut frame)
        .unwrap();
        frame
    }
    fn query(version: u8, group: Ipv4Addr, sources: &[Ipv4Addr], destination: Ipv4Addr) -> Vec<u8> {
        let mut payload = vec![
            0;
            if version == 3 {
                12 + sources.len() * 4
            } else {
                8
            }
        ];
        if version == 3 {
            for (bytes, address) in payload[12..].as_chunks_mut::<4>().0.iter_mut().zip(sources) {
                bytes.copy_from_slice(&address.octets());
            }
        }
        packet_wire::igmp::emit_query(
            &mut payload,
            if version == 2 { 10 } else { 0 },
            group,
            (version == 3).then_some(SourceQueryHeader {
                flags: 0,
                suppress: false,
                robustness: 2,
                query_interval_code: 125,
                source_count: sources.len() as u16,
            }),
        )
        .unwrap();
        envelope(&payload, PEER, destination, version != 1, 1)
    }
    fn check_report(frame: &[u8]) -> IgmpPacket<'_> {
        let ethernet = EthernetFrame::parse(frame).unwrap();
        let ip = Ipv4Packet::parse(ethernet.payload()).unwrap();
        assert_eq!(
            (ip.source(), ip.hop_limit(), ip.options()),
            (LOCAL, 1, &[0x94, 4, 0, 0][..])
        );
        let address = ip.destination().octets();
        assert_eq!(
            ethernet.destination(),
            [1, 0, 0x5e, address[1] & 0x7f, address[2], address[3]]
        );
        IgmpPacket::parse(ip.payload()).unwrap()
    }
    #[test]
    fn ipv4_igmp_membership_delivers_udp_raw_and_retires_only_the_last_reference() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        let udp = scoped_endpoint(&net, InetProtocol::Udp);
        udp.bind(address(44224)).unwrap();
        udp.set_ipv4_multicast_all(false).unwrap();
        udp.set_packet_info(true).unwrap();
        let raw = scoped_endpoint(&net, InetProtocol::Raw(17));
        raw.set_ipv4_multicast_all(false).unwrap();
        let observer = scoped_endpoint(&net, InetProtocol::Raw(17));
        let excluded = scoped_endpoint(&net, InetProtocol::Raw(17));
        excluded.set_ipv4_multicast_all(false).unwrap();
        let mut frame = peer_udp_frame_for(
            InetAddress::new(PEER.into(), 44000),
            InetAddress::new(GROUP.into(), 44224),
            b"multicast",
        );
        let destination = GROUP.octets();
        frame[..6].copy_from_slice(&[
            1,
            0,
            0x5e,
            destination[1] & 0x7f,
            destination[2],
            destination[3],
        ]);
        input(&device, frame.clone());
        poll(&net, 2002);
        assert!(!observer.poll_state().readable);
        crate::allocation_failure::fail_next();
        assert_eq!(
            raw.ipv4_membership(GROUP, 2, Ipv4Addr::UNSPECIFIED, true),
            Err(SocketError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(
            raw.ipv4_membership(PEER, 2, LOCAL, true),
            Err(SocketError::Invalid)
        );
        assert_eq!(
            raw.ipv4_membership(GROUP, 999, LOCAL, true),
            Err(SocketError::NoDevice)
        );
        assert_eq!(
            raw.ipv4_membership(GROUP, 0, Ipv4Addr::UNSPECIFIED, true),
            Err(SocketError::NoDevice),
            "an unspecified IPv4 interface must use a real route"
        );
        raw.ipv4_membership(GROUP, 2, Ipv4Addr::UNSPECIFIED, true)
            .unwrap();
        assert_eq!(
            raw.ipv4_membership(GROUP, 0, Ipv4Addr::UNSPECIFIED, false),
            Err(SocketError::NoDevice)
        );
        assert_eq!(
            raw.ipv4_membership(GROUP, 0, LOCAL, true),
            Err(SocketError::AddressInUse)
        );
        udp.ipv4_membership(GROUP, 0, LOCAL, true).unwrap();
        for ms in [2003, 4000] {
            poll(&net, ms);
        }
        let output = reports(&device);
        assert_eq!(
            output.len(),
            2,
            "one shared interface state-change, not one report per socket"
        );
        for frame in output {
            let IgmpMessage::ReportV3 { mut records, .. } = check_report(&frame).message() else {
                panic!("IGMPv3 join");
            };
            let record = records.next().unwrap();
            assert_eq!(record.record_type(), RecordType::ChangeToExclude as u8);
            assert_eq!(record.group(), IpAddr::V4(GROUP));
            assert_eq!(record.sources().len(), 0);
        }
        input(&device, frame.clone());
        poll(&net, 4001);
        let mut bytes = [0; 80];
        let got = receive(&udp, &mut bytes, false).unwrap();
        assert_eq!(&bytes[..got.0], b"multicast");
        assert_eq!(
            got.3,
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 2,
                specific_address: LOCAL,
                destination: GROUP
            })
        );
        assert_eq!(receive(&raw, &mut bytes, false).unwrap().0, 37);
        receive(&observer, &mut bytes, false).unwrap();
        assert!(!excluded.poll_state().readable);
        raw.ipv4_membership(GROUP, 2, Ipv4Addr::UNSPECIFIED, false)
            .unwrap();
        input(&device, frame.clone());
        poll(&net, 4002);
        assert!(!raw.poll_state().readable);
        receive(&udp, &mut bytes, false).unwrap();
        receive(&observer, &mut bytes, false).unwrap();
        assert!(reports(&device).is_empty());
        udp.close_description();
        input(&device, frame.clone());
        poll(&net, 4003);
        assert!(
            !observer.poll_state().readable,
            "pending leave reports do not prolong receive membership"
        );
        poll(&net, 6000);
        let output = reports(&device);
        assert_eq!(output.len(), 2);
        for frame in output {
            let IgmpMessage::ReportV3 { mut records, .. } = check_report(&frame).message() else {
                panic!("IGMPv3 leave");
            };
            assert_eq!(
                records.next().unwrap().record_type(),
                RecordType::ChangeToInclude as u8
            );
        }
        raw.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        poll(&net, 6001);
        configure_up(&net, 2, false).unwrap();
        configure_up(&net, 2, true).unwrap();
        input(&device, frame);
        poll(&net, 6002);
        assert!(
            raw.poll_state().readable,
            "interface restart rejoins retained socket references"
        );
        for socket in [&raw, &observer, &excluded] {
            socket.close_description();
        }
    }

    #[test]
    fn ipv4_igmp_legacy_suppression_and_late_version_expiry_preserve_interface_state() {
        use core::sync::atomic::Ordering;
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        input(&device, query(2, Ipv4Addr::UNSPECIFIED, &[], ALL));
        poll(&net, 2002);
        let first = scoped_endpoint(&net, InetProtocol::Raw(89));
        first.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        device.transmit_available.store(false, Ordering::Relaxed);
        poll(&net, 2003);
        let mut report = [0; 8];
        packet_wire::igmp::emit_report(&mut report, 0x16, GROUP).unwrap();
        input(
            &device,
            envelope(&report, Ipv4Addr::UNSPECIFIED, GROUP, true, 1),
        );
        poll(&net, 2004);
        device.transmit_available.store(true, Ordering::Relaxed);
        poll(&net, 2005);
        assert!(
            reports(&device).is_empty(),
            "peer report cancels even an unsubmitted legacy join"
        );
        first.close_description();
        poll(&net, 2006);
        assert!(
            reports(&device).is_empty(),
            "a non-last reporter does not send IGMPv2 Leave"
        );
        let second_group = Ipv4Addr::new(239, 2, 2, 2);
        let second = scoped_endpoint(&net, InetProtocol::Raw(89));
        second
            .ipv4_membership(second_group, 2, LOCAL, true)
            .unwrap();
        poll(&net, 2007);
        let output = reports(&device);
        assert_eq!(output.len(), 1);
        assert!(
            matches!(check_report(&output[0]).message(), IgmpMessage::Report { version: 2, group } if group == second_group)
        );
        second.close_description();
        poll(&net, 2008);
        let output = reports(&device);
        assert_eq!(output.len(), 1);
        assert!(
            matches!(check_report(&output[0]).message(), IgmpMessage::Leave { group } if group == second_group)
        );
        assert_eq!(
            Ipv4Packet::parse(EthernetFrame::parse(&output[0]).unwrap().payload())
                .unwrap()
                .destination(),
            Ipv4Addr::new(224, 0, 0, 2)
        );
        input(&device, query(1, Ipv4Addr::UNSPECIFIED, &[], ALL));
        poll(&net, 2009);
        let third = scoped_endpoint(&net, InetProtocol::Raw(89));
        third.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        poll(&net, 2010);
        let output = reports(&device);
        assert_eq!(output.len(), 1);
        assert!(matches!(
            check_report(&output[0]).message(),
            IgmpMessage::Report { version: 1, .. }
        ));
        third.close_description();
        poll(&net, 2011);
        assert!(reports(&device).is_empty(), "IGMPv1 never sends Leave");
        input(&device, query(2, Ipv4Addr::UNSPECIFIED, &[], ALL));
        poll(&net, 200_000);
        let fourth = scoped_endpoint(&net, InetProtocol::Raw(89));
        fourth.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        poll(&net, 200_001);
        let output = reports(&device);
        assert!(output.iter().all(|frame| matches!(
            check_report(frame).message(),
            IgmpMessage::Report { version: 1, .. }
        )));
        poll(&net, 355_000);
        let output = reports(&device);
        assert_eq!(
            output.len(),
            1,
            "late expiry still refreshes current state without duplicate legacy reports"
        );
        assert!(matches!(
            check_report(&output[0]).message(),
            IgmpMessage::Report { version: 2, .. }
        ));
        poll(&net, 465_000);
        let output = reports(&device);
        assert_eq!(output.len(), 1);
        assert!(matches!(
            check_report(&output[0]).message(),
            IgmpMessage::ReportV3 { .. }
        ));
        let valid = query(2, Ipv4Addr::UNSPECIFIED, &[], ALL);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&valid).unwrap().payload()).unwrap();
        input(&device, envelope(ip.payload(), PEER, ALL, false, 1));
        input(&device, envelope(ip.payload(), PEER, ALL, true, 2));
        let mut checksum = query(3, Ipv4Addr::UNSPECIFIED, &[], ALL);
        checksum[40] ^= 1;
        input(&device, checksum);
        input(&device, query(1, Ipv4Addr::UNSPECIFIED, &[], GROUP));
        poll(&net, 465_001);
        poll(&net, 480_000);
        assert!(
            reports(&device).is_empty(),
            "missing RA, wrong TTL/checksum or general-query destination cannot change querier mode"
        );
        fourth.close_description();
    }

    #[test]
    fn ipv4_igmp_source_queries_merge_split_at_mtu_and_do_not_filter_asm_data() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        configure_mtu(&net, 2, 68).unwrap();
        let udp = scoped_endpoint(&net, InetProtocol::Udp);
        udp.bind(address(44224)).unwrap();
        udp.set_ipv4_multicast_all(false).unwrap();
        udp.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        for ms in [2002, 4000] {
            poll(&net, ms);
        }
        reports(&device);
        let first = (1..=8)
            .rev()
            .map(|last| Ipv4Addr::new(198, 51, 100, last))
            .collect::<Vec<_>>();
        let second = (5..=12)
            .map(|last| Ipv4Addr::new(198, 51, 100, last))
            .collect::<Vec<_>>();
        input(&device, query(3, GROUP, &first, GROUP));
        input(&device, query(3, GROUP, &second, GROUP));
        input(&device, query(3, GROUP, &[Ipv4Addr::UNSPECIFIED], GROUP));
        input(&device, query(3, Ipv4Addr::UNSPECIFIED, &[PEER], ALL));
        poll(&net, 5001);
        assert!(reports(&device).is_empty());
        poll(&net, 5002);
        let output = reports(&device);
        assert_eq!(output.len(), 2);
        let mut sources = Vec::new();
        for frame in &output {
            assert!(frame.len() <= 14 + 68);
            let IgmpMessage::ReportV3 { mut records, .. } = check_report(frame).message() else {
                panic!("source-query reply");
            };
            let record = records.next().unwrap();
            assert_eq!(record.record_type(), RecordType::ModeIsInclude as u8);
            assert_eq!(record.group(), IpAddr::V4(GROUP));
            sources.extend(record.sources());
        }
        assert_eq!(
            sources,
            (1..=12)
                .map(|last| IpAddr::V4(Ipv4Addr::new(198, 51, 100, last)))
                .collect::<Vec<_>>()
        );
        let mut frame = peer_udp_frame_for(
            InetAddress::new(PEER.into(), 44000),
            InetAddress::new(GROUP.into(), 44224),
            b"asm",
        );
        frame[..6].copy_from_slice(&[1, 0, 0x5e, 1, 1, 1]);
        input(&device, frame);
        poll(&net, 5003);
        let mut bytes = [0; 8];
        let result = receive(&udp, &mut bytes, false).unwrap();
        assert_eq!(
            &bytes[..result.0],
            b"asm",
            "query sources are transient report state, not an application filter"
        );
        udp.close_description();
    }
    // 最终 NIC 提交与 lo 消费的先后、TTL 字节位置及重试去重需要实际报文观察，静态审查不足。
    // 继续使用同一 NET/NIC fixture；不新增生产采样或另一套发送实现。
    #[test]
    fn ipv4_multicast_udp_freezes_policy_and_loops_once_before_blocked_nic() {
        use core::sync::atomic::Ordering;
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        let receiver = scoped_endpoint(&net, InetProtocol::Udp);
        receiver.bind(address(44224)).unwrap();
        receiver.set_ipv4_multicast_all(false).unwrap();
        receiver.set_packet_info(true).unwrap();
        receiver.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        let raw = scoped_endpoint(&net, InetProtocol::Raw(17));
        raw.set_ipv4_multicast_all(false).unwrap();
        raw.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        poll(&net, 2002);
        poll(&net, 4000);
        device.wire.lock().unwrap().1.clear();
        let sender = scoped_endpoint(&net, InetProtocol::Udp);
        let target = Some(InetAddress::new(GROUP.into(), 44224));
        assert_eq!(sender.option(InternetOption::Ipv4MulticastInterface), Ok(0));
        assert_eq!(sender.option(InternetOption::Ipv4MulticastHopLimit), Ok(1));
        assert_eq!(sender.option(InternetOption::Ipv4MulticastLoop), Ok(1));
        assert_eq!(
            sender.send_to(b"no route", target, &[]),
            Err(SocketError::NetworkUnreachable)
        );
        sender.set_ipv4_multicast_interface(0, LOCAL).unwrap();
        assert_eq!(
            sender.option(InternetOption::Ipv4MulticastInterface),
            Ok(u32::from(LOCAL) as i32)
        );
        assert_eq!(
            sender.set_ipv4_multicast_interface(999, LOCAL),
            Err(SocketError::AddressNotAvailable)
        );
        sender.set_hop_limit(Some(77)).unwrap();
        device.transmit_available.store(false, Ordering::Relaxed);
        sender.send_to(b"once", target, &[]).unwrap();
        sender
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastLoop, 0)
            .unwrap();
        sender
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastHopLimit, 0)
            .unwrap();
        for ms in 4001..4005 {
            poll(&net, ms);
        }
        let mut bytes = [0; 100];
        let got = receive(&receiver, &mut bytes, false).unwrap();
        assert_eq!(&bytes[..got.0], b"once");
        assert_eq!(
            got.3,
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 2,
                specific_address: LOCAL,
                destination: GROUP
            })
        );
        let got = receive(&raw, &mut bytes, false).unwrap();
        let ip = Ipv4Packet::parse(&bytes[..got.0]).unwrap();
        assert_eq!(
            (ip.source(), ip.destination(), ip.hop_limit()),
            (LOCAL, GROUP, 1)
        );
        packet_wire::udp::UdpPacket::parse(ip.payload(), LOCAL.into(), GROUP.into()).unwrap();
        assert!(!receiver.poll_state().readable);
        assert!(device.wire.lock().unwrap().1.is_empty());
        device.transmit_available.store(true, Ordering::Relaxed);
        for ms in [6000, 6001] {
            poll(&net, ms);
        }
        let output = core::mem::take(&mut device.wire.lock().unwrap().1);
        assert_eq!(output.len(), 1);
        let ethernet = EthernetFrame::parse(&output[0]).unwrap();
        assert_eq!(ethernet.destination(), [1, 0, 0x5e, 1, 1, 1]);
        let ip = Ipv4Packet::parse(ethernet.payload()).unwrap();
        assert_eq!(ip.hop_limit(), 1);
        packet_wire::udp::UdpPacket::parse(ip.payload(), LOCAL.into(), GROUP.into()).unwrap();
        assert!(!receiver.poll_state().readable && !raw.poll_state().readable);
        // 新包采用新的 TTL/loop 策略；TTL 0 不能上链路，loop 关闭不能投递本机。
        sender.send_to(b"neither", target, &[]).unwrap();
        poll(&net, 6002);
        poll(&net, 6003);
        assert!(device.wire.lock().unwrap().1.is_empty());
        assert!(!receiver.poll_state().readable);
        sender
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastLoop, 1)
            .unwrap();
        sender.send_to(b"local", target, &[]).unwrap();
        poll(&net, 6004);
        poll(&net, 6005);
        assert!(device.wire.lock().unwrap().1.is_empty());
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 5);
        let got = receive(&raw, &mut bytes, false).unwrap();
        assert_eq!(Ipv4Packet::parse(&bytes[..got.0]).unwrap().hop_limit(), 0);
        sender
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastHopLimit, -1)
            .unwrap();
        assert_eq!(sender.option(InternetOption::Ipv4MulticastHopLimit), Ok(1));
        assert_eq!(sender.option(InternetOption::HopLimit), Ok(77));
        // 单播继续采用单播 TTL；组播默认接口和回送选项不会限制本机单播。
        sender
            .send_to(b"unicast", Some(InetAddress::new(LOCAL.into(), 44224)), &[])
            .unwrap();
        poll(&net, 6006);
        poll(&net, 6007);
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 7);
        let got = receive(&raw, &mut bytes, false).unwrap();
        assert_eq!(Ipv4Packet::parse(&bytes[..got.0]).unwrap().hop_limit(), 77);
        let mapped = InetSocket::new(
            network_protocol::InetFamily::Ipv6,
            InetProtocol::Udp,
            Arc::new(crate::Event::default()),
            net.clone(),
        )
        .unwrap();
        mapped
            .set_ipv4_multicast_interface(2, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        mapped
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastHopLimit, 0)
            .unwrap();
        mapped
            .set_ipv6_option(InternetOption::Ipv6MulticastLoop, 0)
            .unwrap();
        mapped
            .send_to(
                b"mapped",
                Some(InetAddress::new(GROUP.to_ipv6_mapped().into(), 44224)),
                &[],
            )
            .unwrap();
        poll(&net, 6008);
        poll(&net, 6009);
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 6);
        let got = receive(&raw, &mut bytes, false).unwrap();
        assert_eq!(Ipv4Packet::parse(&bytes[..got.0]).unwrap().hop_limit(), 0);
        mapped.close_description();
        // 实际 lo 只自行投递一次，loop 标志不会制造第二份报文。
        receiver
            .ipv4_membership(GROUP, 1, Ipv4Addr::UNSPECIFIED, true)
            .unwrap();
        sender
            .set_ipv4_multicast_interface(1, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        sender.send_to(b"lo", target, &[]).unwrap();
        poll(&net, 6010);
        poll(&net, 6011);
        assert_eq!(receive(&receiver, &mut bytes, false).unwrap().0, 2);
        assert!(!receiver.poll_state().readable);
        sender
            .set_ipv4_multicast_interface(0, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        assert_eq!(
            sender.send_to(b"reset", target, &[]),
            Err(SocketError::NetworkUnreachable)
        );
        sender.close_description();
        raw.close_description();
        receiver.close_description();
    }

    #[test]
    fn ipv4_multicast_raw_selects_interface_and_uses_included_header_ttl() {
        use network_protocol::InternetSendPacketInfo;
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let net = crate::namespace::Namespace::create().unwrap();
        let first = Arc::new(WireDevice::new());
        let second = Arc::new(WireDevice::new());
        let devices = [
            first.clone() as Arc<dyn device_model::network::NetworkDevice>,
            second.clone(),
        ];
        net.install_protocol(prepare_network(&net, &devices, true).unwrap());
        let other = Ipv4Addr::new(192, 0, 2, 1);
        for (index, address) in [(2, LOCAL), (3, other)] {
            configure_address(&net, index, address).unwrap();
            configure_netmask(&net, index, Ipv4Addr::new(255, 255, 255, 0)).unwrap();
            configure_up(&net, index, true).unwrap();
        }
        for ms in [0, 1000, 2000, 2001] {
            poll(&net, ms);
        }
        let receiver = scoped_endpoint(&net, InetProtocol::Raw(89));
        receiver.set_packet_info(true).unwrap();
        receiver.set_ipv4_multicast_all(false).unwrap();
        receiver.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        receiver.ipv4_membership(GROUP, 3, other, true).unwrap();
        poll(&net, 2002);
        poll(&net, 4000);
        first.wire.lock().unwrap().1.clear();
        second.wire.lock().unwrap().1.clear();
        let sender = scoped_endpoint(&net, InetProtocol::Raw(89));
        sender.set_ipv4_multicast_all(false).unwrap();
        sender
            .set_ipv4_multicast_interface(3, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        let target = Some(InetAddress::new(GROUP.into(), 0));
        sender.send_to(b"route", target, &[]).unwrap();
        poll(&net, 4001);
        poll(&net, 4002);
        assert!(first.wire.lock().unwrap().1.is_empty());
        let output = core::mem::take(&mut second.wire.lock().unwrap().1);
        assert_eq!(output.len(), 1);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&output[0]).unwrap().payload()).unwrap();
        assert_eq!((ip.source(), ip.hop_limit()), (other, 1));
        let mut bytes = [0; 100];
        assert_eq!(
            receive(&receiver, &mut bytes, false).unwrap().3,
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 3,
                specific_address: other,
                destination: GROUP
            })
        );
        let control = [InternetSendPacketInfo::Ipv4 {
            interface_index: Some(2),
            source_address: LOCAL,
        }];
        sender.send_to(b"override", target, &control).unwrap();
        poll(&net, 4003);
        poll(&net, 4004);
        assert_eq!(first.wire.lock().unwrap().1.len(), 1);
        first.wire.lock().unwrap().1.clear();
        assert!(second.wire.lock().unwrap().1.is_empty());
        assert_eq!(
            receive(&receiver, &mut bytes, false).unwrap().3,
            Some(InternetPacketInfo::Ipv4 {
                interface_index: 2,
                specific_address: LOCAL,
                destination: GROUP
            })
        );
        // HDRINCL 的 TTL 在 IPv4 第 8 字节；第 7 字节属于 fragment flags，不能据此判定。
        sender.set_header_included(true).unwrap();
        sender
            .set_ipv4_multicast_option(InternetOption::Ipv4MulticastHopLimit, 9)
            .unwrap();
        let mut packet = vec![0; 24];
        packet[20..].copy_from_slice(b"head");
        Ipv4Header {
            source: Ipv4Addr::UNSPECIFIED,
            destination: GROUP,
            protocol: 89,
            hop_limit: 0,
            dscp_ecn: 0,
            identification: 0,
            fragment: 0x4000,
            options: &[],
        }
        .emit(&mut packet)
        .unwrap();
        sender.send_to(&packet, target, &control).unwrap();
        poll(&net, 4005);
        poll(&net, 4006);
        assert!(
            first.wire.lock().unwrap().1.is_empty() && second.wire.lock().unwrap().1.is_empty()
        );
        let got = receive(&receiver, &mut bytes, false).unwrap();
        assert_eq!(Ipv4Packet::parse(&bytes[..got.0]).unwrap().hop_limit(), 0);
        packet[8] = 2;
        sender.send_to(&packet, target, &control).unwrap();
        poll(&net, 4007);
        poll(&net, 4008);
        assert_eq!(first.wire.lock().unwrap().1.len(), 1);
        receive(&receiver, &mut bytes, false).unwrap();
        assert!(!sender.poll_state().readable);
        // 非零 ifindex 的地址只是选源意图，设置可成功；实际发包仍拒绝非本机源。
        sender.set_header_included(false).unwrap();
        sender.set_ipv4_multicast_interface(3, PEER).unwrap();
        assert_eq!(
            sender.send_to(b"invalid-source", target, &[]),
            Err(SocketError::NetworkUnreachable)
        );
        sender.close_description();
        receiver.close_description();
    }

    #[test]
    fn ipv4_multicast_looped_igmp_query_updates_original_interface() {
        let _serial = SERIAL.lock().unwrap();
        initialize();
        let (net, device) = domain();
        let sender = scoped_endpoint(&net, InetProtocol::Raw(2));
        sender
            .set_ipv4_multicast_interface(2, Ipv4Addr::UNSPECIFIED)
            .unwrap();
        let query = query(1, Ipv4Addr::UNSPECIFIED, &[], ALL);
        let ip = Ipv4Packet::parse(EthernetFrame::parse(&query).unwrap().payload()).unwrap();
        sender
            .send_to(ip.payload(), Some(InetAddress::new(ALL.into(), 0)), &[])
            .unwrap();
        poll(&net, 2002);
        poll(&net, 2003);
        device.wire.lock().unwrap().1.clear();
        let receiver = scoped_endpoint(&net, InetProtocol::Raw(89));
        receiver.ipv4_membership(GROUP, 2, LOCAL, true).unwrap();
        poll(&net, 2004);
        let output = reports(&device);
        assert_eq!(output.len(), 1);
        assert!(matches!(
            check_report(&output[0]).message(),
            IgmpMessage::Report {
                version: 1,
                group: GROUP
            }
        ));
        sender.close_description();
        receiver.close_description();
    }
}
