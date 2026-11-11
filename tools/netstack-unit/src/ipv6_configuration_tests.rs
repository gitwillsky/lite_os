// 多地址的实际 NET/RIB 配置、DAD、共享组及逆向时序无法由单链路地址用例证明。
// 仅构造配置和对端标准报文；地址、FIB、邻居与 socket 都执行生产 owner。
use router_protocol::{
    configuration::Ipv6Address,
    interface::{InterfaceConfiguration, Ipv6Addresses},
};
const GLOBAL: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 0xabcd);
const PRIVATE: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 1, 0, 0, 0, 0, 0xabcd);
const GLOBAL_PEER: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2);
fn definition(address: Ipv6Addr, length: u8, on_link: bool) -> Ipv6Address {
    Ipv6Address {
        address,
        prefix_length: length,
        on_link,
    }
}
fn plan(index: u32, addresses: &[Ipv6Address]) -> InterfaceConfiguration<'_> {
    InterfaceConfiguration {
        index,
        ipv6: Some(Ipv6Addresses::new(addresses).unwrap()),
        delegated: None,
        accept_ra: None,
        advertise: None,
        up: true,
        mtu: None,
        address: (index == 1).then_some(Ipv4Addr::LOCALHOST),
        prefix_length: if index == 1 { 8 } else { 0 },
        route: None,
    }
}
fn apply(net: &Arc<crate::namespace::Namespace>, plans: &[InterfaceConfiguration<'_>]) {
    let transaction = prepare_configuration(net, plans.iter().copied(), None).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
}
fn row(net: &crate::namespace::Namespace, address: Ipv6Addr) -> Option<InterfaceAddress> {
    let snapshot = address_snapshot(net, 0).unwrap();
    (0..snapshot.len())
        .filter_map(|index| snapshot.get(index))
        .find(|row| row.address == address)
}
fn socket6(net: &Arc<crate::namespace::Namespace>) -> Arc<InetSocket> {
    InetSocket::new(
        network_protocol::InetFamily::Ipv6,
        InetProtocol::Udp,
        Arc::new(crate::Event::default()),
        net.clone(),
    )
    .unwrap()
}
fn group(address: Ipv6Addr) -> Ipv6Addr {
    let bytes = address.octets();
    Ipv6Addr::new(
        0xff02,
        0,
        0,
        0,
        0,
        1,
        0xff00 | u16::from(bytes[13]),
        u16::from_be_bytes([bytes[14], bytes[15]]),
    )
}
fn request(source: Ipv6Addr, target: Ipv6Addr) -> Vec<u8> {
    let group = group(target);
    let mut frame = vec![0; 86];
    OptionValue::LinkLayerAddress {
        source: true,
        address: &MAC,
    }
    .emit(&mut frame[78..])
    .unwrap();
    NdMessage::NeighborSolicitation { target }
        .emit(&mut frame[54..], source, group)
        .unwrap();
    envelope(&mut frame, source, group, false, 255);
    frame
}
fn datagram(source: Ipv6Addr, target: Ipv6Addr, port: u16) -> Vec<u8> {
    let mut frame = vec![0; 65];
    frame[62..].copy_from_slice(b"udp");
    packet_wire::udp::UdpHeader {
        source: source.into(),
        destination: target.into(),
        source_port: 44000,
        destination_port: port,
    }
    .emit_prefix(&mut frame[54..62], b"udp", true)
    .unwrap();
    envelope(&mut frame, source, target, false, 64);
    frame[20] = 17;
    frame
}
fn advance_configuration(net: &crate::namespace::Namespace, start: i64) {
    for at in [start, start + 1000, start + 2000, start + 2001] {
        poll(net, at);
    }
}
#[test]
fn static6_reaches_the_real_rib_batch_dad_source_selection_and_global_udp_paths() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, true), definition(PRIVATE, 64, true)];
    let loopback: Ipv6Addr = "2001:db8:3::1".parse().unwrap();
    let local = [definition(loopback, 128, true)];
    let plans = [plan(1, &local), plan(2, &addresses)];
    let rib = routing_rib::Catalogue::<u32, (), routing_rib::NextHop>::build(vec![])
        .unwrap()
        .resolve(|_| true, |_| true)
        .unwrap();
    let bytes = rib.encode_configuration(&plans, 254, |_| None).unwrap();
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).unwrap();
    assert!(batch.interfaces().iter().eq(plans));
    let forwarding = batch.forwarding().unwrap();
    let transaction = prepare_routes(
        &net,
        batch.interfaces().iter(),
        forwarding.next_hops(),
        forwarding.iter(),
        None,
    )
    .unwrap();
    assert!(row(&net, GLOBAL).is_none());
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    assert_eq!(row(&net, loopback).unwrap().state, State::Preferred);
    poll(&net, 2002);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Tentative);
    let server = socket6(&net);
    assert_eq!(
        server.bind(InetAddress::new(GLOBAL.into(), 43500)),
        Err(SocketError::AddressNotAvailable)
    );
    for at in [3002, 4002, 4003] {
        poll(&net, at);
    }
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    assert_eq!(row(&net, PRIVATE).unwrap().prefix_length, 64);
    frames(&device);
    server.bind(InetAddress::new(GLOBAL.into(), 43500)).unwrap();
    let client = socket6(&net);
    let private_peer: Ipv6Addr = "fd00:0:1::2".parse().unwrap();
    for (source, target) in [(GLOBAL_PEER, GLOBAL), (private_peer, PRIVATE)] {
        enqueue(&device, request(source, target));
    }
    poll(&net, 4004);
    let replies = frames(&device);
    assert_eq!(replies.iter().filter(|frame| NdPacket::parse(parse(frame).1).is_ok_and(|packet|
        matches!(packet.message(), NdMessage::NeighborAdvertisement { target, .. } if target == GLOBAL || target == PRIVATE))).count(), 2);
    client
        .send_to(
            b"global",
            Some(InetAddress::new(GLOBAL_PEER.into(), 44000)),
            &[],
        )
        .unwrap();
    client
        .send_to(
            b"private",
            Some(InetAddress::new(private_peer.into(), 44000)),
            &[],
        )
        .unwrap();
    poll(&net, 4005);
    let output = frames(&device);
    for (destination, source) in [(GLOBAL_PEER, GLOBAL), (private_peer, PRIVATE)] {
        let packet = output
            .iter()
            .map(|frame| Ipv6Packet::parse(&frame[14..]).unwrap())
            .find(|ip| ip.upper_protocol() == 17 && ip.destination() == destination)
            .unwrap();
        assert_eq!(packet.source(), source);
    }
    enqueue(&device, datagram(GLOBAL_PEER, GLOBAL, 43500));
    enqueue(&device, echo(GLOBAL_PEER, GLOBAL, b"global-ping"));
    poll(&net, 4006);
    assert_eq!(
        udp_read(&server, false).unwrap(),
        (
            b"udp".to_vec(),
            Some(InetAddress::new(GLOBAL_PEER.into(), 44000))
        )
    );
    let output = frames(&device);
    assert!(output.iter().any(|frame| {
        let (ip, icmp) = parse(frame);
        ip.source() == GLOBAL && ip.destination() == GLOBAL_PEER && icmp.message_type() == 129
    }));
    let local_server = socket6(&net);
    local_server
        .bind(InetAddress::new(loopback.into(), 43501))
        .unwrap();
    client
        .send_to(
            b"local",
            Some(InetAddress::new(loopback.into(), 43501)),
            &[],
        )
        .unwrap();
    poll(&net, 4007);
    poll(&net, 4008);
    assert_eq!(udp_read(&local_server, false).unwrap().0, b"local");
    for endpoint in [server, client, local_server] {
        endpoint.close_description();
    }
}
#[test]
fn static6_rollback_keeps_common_dad_and_shared_mld_but_reprobes_reassigned_addresses() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let previous = [definition(GLOBAL, 64, true), definition(PRIVATE, 64, true)];
    apply(&net, &[plan(2, &previous)]);
    advance_configuration(&net, 2002);
    frames(&device);
    let added: Ipv6Addr = "2001:db8:2::1234".parse().unwrap();
    let next = [definition(GLOBAL, 80, false), definition(added, 64, true)];
    let transaction = prepare_configuration(&net, [plan(2, &next)].into_iter(), None).unwrap();
    crate::allocation_failure::fail_next();
    transaction.activate().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    assert!(!row(&net, GLOBAL).unwrap().on_link);
    assert!(row(&net, PRIVATE).is_none());
    let inserted_device = Arc::new(WireDevice::new());
    let inserted = install_interface(&net, inserted_device.clone(), crate::Channel, crate::Event::default()).unwrap();
    advance_configuration(&net, 4004);
    assert_eq!(row(&net, added).unwrap().state, State::Preferred);
    frames(&device);
    crate::allocation_failure::fail_next();
    transaction.abort().unwrap();
    assert!(!crate::allocation_failure::was_consumed());
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    assert_eq!(row(&net, GLOBAL).unwrap().prefix_length, 64);
    assert!(row(&net, added).is_none());
    poll(&net, 6006);
    assert_eq!(row(&net, PRIVATE).unwrap().state, State::Tentative);
    let shared = group(GLOBAL);
    let removed = group(added);
    let output = frames(&device);
    let mut left_removed = false;
    for frame in &output {
        let (_, icmp) = parse(frame);
        if let Ok(MldMessage::ReportV2 { records, .. }) = MldMessage::parse(icmp) {
            for record in records {
                if record.record_type() == RecordType::ChangeToInclude as u8 {
                    assert_ne!(
                        record.group(),
                        core::net::IpAddr::V6(shared),
                        "one shared-group user remains assigned"
                    );
                    left_removed |= record.group() == removed;
                }
            }
        }
    }
    assert!(
        left_removed,
        "rollback must finish the newly assigned group's leave obligation"
    );
    poll(&net, 7006);
    poll(&net, 8006);
    assert_eq!(row(&net, PRIVATE).unwrap().state, State::Preferred);
    configure_up(&net, inserted, true).unwrap();
    advance_configuration(&net, 8007);
    let network = stack(&net).unwrap().lock().unwrap();
    assert_eq!(
        network
            .ipv6_routing
            .select(
                Ipv6Addr::UNSPECIFIED,
                PEER,
                Some(inserted),
                0,
                &network.interfaces
            )
            .unwrap()
            .interface,
        inserted,
        "rollback must retain the newly inserted interface's automatic prefix"
    );
}
#[test]
fn static6_does_not_infer_onlink_routes_and_ipv4_lease_updates_keep_ipv6() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, false)];
    apply(&net, &[plan(2, &addresses)]);
    advance_configuration(&net, 2002);
    frames(&device);
    let endpoint = socket6(&net);
    endpoint
        .bind(InetAddress::new(GLOBAL.into(), 43502))
        .unwrap();
    assert_eq!(
        endpoint.send_to(
            b"no implied route",
            Some(InetAddress::new(GLOBAL_PEER.into(), 44000)),
            &[]
        ),
        Err(SocketError::NetworkUnreachable)
    );
    let mut lease = plan(2, &[]);
    lease.ipv6 = None;
    apply(&net, &[lease]);
    assert_eq!(row(&net, GLOBAL).unwrap().state, State::Preferred);
    endpoint
        .send_to(b"local", Some(InetAddress::new(GLOBAL.into(), 43502)), &[])
        .unwrap();
    poll(&net, 4004);
    poll(&net, 4005);
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"local");
    apply(&net, &[plan(2, &[])]);
    assert!(row(&net, GLOBAL).is_none());
    assert_eq!(row(&net, LOCAL).unwrap().state, State::Preferred);
    endpoint.close_description();
}

#[test]
fn static6_control_budget_yields_without_starving_later_groups_or_addresses() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = (0..12)
        .map(|last| {
            definition(
                Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 0x100 + last),
                64,
                true,
            )
        })
        .collect::<Vec<_>>();
    apply(&net, &[plan(2, &addresses)]);
    for ms in [2002, 3002, 4002, 5002] {
        let mut drained = false;
        for _ in 0..16 {
            let outcome = stack(&net)
                .unwrap()
                .lock()
                .unwrap()
                .poll_at(Instant::from_millis(ms));
            let output = frames(&device);
            let dad = output
                .iter()
                .filter(|frame| parse(frame).1.message_type() == 135)
                .count();
            let reports = output
                .iter()
                .filter(|frame| parse(frame).1.message_type() == 143)
                .count();
            assert!(dad <= 4 && reports <= 4);
            if !outcome.backlog {
                drained = true;
                break;
            }
        }
        assert!(
            drained,
            "capacity without pending work must not cause a poll loop"
        );
    }
    for address in addresses {
        assert_eq!(row(&net, address.address).unwrap().state, State::Preferred);
    }
}

// 单 NIC 路径不能发现“全局源可绑定另一接口，但回复在入接口被丢弃”的跨接口资格不一致。
#[test]
fn static6_global_addresses_belong_to_net_while_nd_and_path_mtu_keep_their_interface_scope() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    let other_device = Arc::new(WireDevice::new());
    let other = install_interface(&net, other_device, crate::Channel, crate::Event::default()).unwrap();
    let foreign: Ipv6Addr = "2001:db8:3::1".parse().unwrap();
    let first = [definition(GLOBAL, 64, true)];
    let second = [definition(foreign, 64, true)];
    apply(&net, &[plan(2, &first), plan(other, &second)]);
    advance_configuration(&net, 0);
    frames(&device);
    let endpoint = socket6(&net);
    endpoint
        .bind(InetAddress::new(foreign.into(), 43503))
        .unwrap();
    endpoint
        .connect(InetAddress::new(GLOBAL_PEER.into(), 44000))
        .unwrap();
    enqueue(&device, request(GLOBAL_PEER, GLOBAL));
    poll(&net, 2002);
    frames(&device);
    enqueue(&device, datagram(GLOBAL_PEER, foreign, 43503));
    enqueue(
        &device,
        echo(GLOBAL_PEER, foreign, b"local-on-another-interface"),
    );
    poll(&net, 2003);
    assert_eq!(udp_read(&endpoint, false).unwrap().0, b"udp");
    assert!(frames(&device).iter().any(|frame| {
        let (ip, icmp) = parse(frame);
        ip.source() == foreign && ip.destination() == GLOBAL_PEER && icmp.message_type() == 129
    }));
    // 接收到发给另一接口地址的 NS 也不得代替它回答，避免把弱主机收发误变成 proxy ND。
    let mut nd = request(GLOBAL_PEER, foreign);
    NdMessage::NeighborSolicitation { target: foreign }
        .emit(&mut nd[54..], GLOBAL_PEER, foreign)
        .unwrap();
    envelope(&mut nd, GLOBAL_PEER, foreign, false, 255);
    enqueue(&device, nd);
    enqueue(
        &device,
        echo(GLOBAL_PEER, Ipv6Addr::LOCALHOST, b"not-from-the-wire"),
    );
    poll(&net, 2004);
    assert!(frames(&device).is_empty());
    endpoint.send_to(&[0x5a; 1300], None, &[]).unwrap();
    poll(&net, 2005);
    let output = frames(&device);
    let original = output
        .iter()
        .map(|frame| Ipv6Packet::parse(&frame[14..]).unwrap())
        .find(|ip| ip.upper_protocol() == 17)
        .unwrap();
    assert_eq!(original.source(), foreign);
    let mut error = vec![0; 110];
    error[58..62].copy_from_slice(&1280u32.to_be_bytes());
    error[62..].copy_from_slice(&original.bytes()[..48]);
    packet_wire::icmpv6::emit(&mut error[54..], 2, 0, GLOBAL_PEER, foreign).unwrap();
    envelope(&mut error, GLOBAL_PEER, foreign, false, 64);
    enqueue(&device, error);
    poll(&net, 2006);
    assert_eq!(endpoint.take_error(), Some(SocketError::MessageTooLarge));
    assert_eq!(endpoint.option(InternetOption::Ipv6Mtu), Ok(1280));
    configure_mtu(&net, 2, 1400).unwrap();
    assert_eq!(
        endpoint.option(InternetOption::Ipv6Mtu),
        Ok(1280),
        "unrelated interface observation must retain a valid NET-local source"
    );
    configure_up(&net, other, false).unwrap();
    assert_eq!(
        endpoint.option(InternetOption::Ipv6Mtu),
        Err(SocketError::NetworkUnreachable)
    );
    endpoint.close_description();
}

// 路由执行与快照寿命沿独立领域组织，复用上面的实际地址/NIC 输入。
mod routes {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ipv6_route_tests.rs"
    ));
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ipv6_failover_tests.rs"
    ));
}

mod bgp_contract {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ipv6_bgp_tests.rs"
    ));
}

mod advertisements {
    use super::*;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ipv6_advertisement_tests.rs"
    ));
}

// 同一真实邻居进入和离开 Failed 均须通知，探测过程不能冒充拓扑变化。
#[test]
fn routing_observer_distinguishes_ipv6_gateway_failure_and_recovery_from_topology() {
    use core::sync::atomic::Ordering;
    use network_protocol::NetworkInstaller;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let net = crate::namespace::Namespace::create().unwrap();
    net.install_protocol(prepare_network(&net, &[], true).unwrap());
    let changes = Arc::new(crate::Event::default());
    observe_interfaces(&net, changes.clone()).unwrap();
    let device = Arc::new(WireDevice::new());
    let index = install_interface(&net, device.clone(), crate::Channel, crate::Event::default()).unwrap();
    let addresses = [definition(GLOBAL, 64, true)];
    apply(&net, &[InterfaceConfiguration {
        accept_ra: Some(router_protocol::configuration::RouterAdvertisementPolicy::Always),
        ..plan(index, &addresses)
    }]);
    advance_configuration(&net, 0);
    let mut ra = vec![0; 70];
    NdMessage::RouterAdvertisement {
        hop_limit: 0, flags: 0, lifetime: 60, reachable_time: 0, retransmit_timer: 0,
    }.emit(&mut ra[54..], PEER, ALL).unwrap();
    envelope(&mut ra, PEER, ALL, false, 255);
    enqueue(&device, ra);
    poll(&net, 4003);
    assert_eq!(changes.0.load(Ordering::Relaxed), NetworkInstaller::TOPOLOGY_CHANGED);
    changes.signal(NetworkInstaller::TOPOLOGY_CHANGED, 0).unwrap();

    let socket = socket6(&net);
    let remote = InetAddress::new("2001:db8:900::9".parse().unwrap(), 44000);
    socket.send_to(b"probe", Some(remote), &[]).unwrap();
    for time in [4004, 4004, 5004, 6004] {
        poll(&net, time);
    }
    assert_eq!(changes.0.load(Ordering::Relaxed), 0);
    poll(&net, 7004);
    assert_eq!(changes.0.load(Ordering::Relaxed), NetworkInstaller::GATEWAYS_CHANGED);
    let neighbors = || crate::inet::configuration::neighbor_snapshot(&net, index).unwrap();
    assert_eq!(neighbors().get(0).unwrap().state, network_protocol::NeighborState::Failed);
    changes.signal(NetworkInstaller::GATEWAYS_CHANGED, 0).unwrap();
    poll(&net, 7005);
    assert_eq!(changes.0.load(Ordering::Relaxed), 0, "retained failure is not another transition");

    let mut na = vec![0; 86];
    OptionValue::LinkLayerAddress { source: false, address: &MAC }
        .emit(&mut na[78..]).unwrap();
    NdMessage::NeighborAdvertisement { flags: 0xe000_0000, target: PEER }
        .emit(&mut na[54..], PEER, LOCAL).unwrap();
    envelope(&mut na, PEER, LOCAL, false, 255);
    enqueue(&device, na);
    poll(&net, 7006);
    assert_eq!(changes.0.load(Ordering::Relaxed), NetworkInstaller::GATEWAYS_CHANGED);
    assert_eq!(neighbors().get(0).unwrap().state, network_protocol::NeighborState::Reachable);
    socket.close_description();
}

// 两个接收接口共用 NET 的转发开关。真实报文验证策略只作用于配置的接口，回滚恢复原资格。
#[test]
fn router_advertisement_policy_is_per_interface_and_rolls_back_with_forwarding() {
    use router_protocol::configuration::RouterAdvertisementPolicy as Policy;

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, uplink) = domain();
    let downstream = Arc::new(WireDevice::new());
    let other = install_interface(&net, downstream.clone(), crate::Channel, crate::Event::default()).unwrap();
    configure_up(&net, other, true).unwrap();
    ready(&net, &uplink);
    frames(&downstream);

    let policy = |index, value| InterfaceConfiguration {
        ipv6: None,
        delegated: None,
        accept_ra: Some(value),
        ..plan(index, &[])
    };
    let transaction = prepare_configuration(
        &net,
        [policy(2, Policy::Always), policy(other, Policy::Disabled)].into_iter(),
        None,
    ).unwrap();
    transaction.activate().unwrap();
    transaction.confirm().unwrap();
    poll(&net, 2002);
    poll(&net, 3002);
    let sent_request = |device: &WireDevice| {
        frames(device).iter().any(|frame| {
            let (_, icmp) = parse(frame);
            NdPacket::parse(icmp).is_ok_and(|packet| packet.message() == NdMessage::RouterSolicitation)
        })
    };
    assert!(sent_request(&uplink));
    assert!(!sent_request(&downstream));

    let advertise = |device: &WireDevice, source| {
        let mut frame = vec![0; 70];
        NdMessage::RouterAdvertisement { hop_limit: 0, flags: 0, lifetime: 1, reachable_time: 0, retransmit_timer: 0 }
            .emit(&mut frame[54..], source, ALL).unwrap();
        envelope(&mut frame, source, ALL, false, 255);
        enqueue(device, frame);
    };
    let routes = || {
        super::super::configuration::route_snapshot::<Ipv6Addr>(&net).unwrap().iter()
            .filter(|row| row.route.protocol == 9)
            .map(|row| row.single_path().unwrap().interface_index.unwrap())
            .collect::<Vec<_>>()
    };
    advertise(&uplink, LOCAL);
    advertise(&downstream, PEER);
    poll(&net, 3003);
    assert!(routes().is_empty());
    advertise(&uplink, PEER);
    poll(&net, 3004);
    assert_eq!(routes(), [2]);
    assert!(stack(&net).unwrap().lock().unwrap().forwarding_enabled.ipv6);

    let transaction = prepare_configuration(
        &net,
        [policy(2, Policy::Disabled)].into_iter(),
        Some(network_protocol::Forwarding { ipv4: true, ipv6: false }),
    ).unwrap();
    transaction.activate().unwrap();
    assert_eq!(routes(), [2], "existing routes keep their announced lifetime");
    advertise(&uplink, PEER);
    advertise(&downstream, PEER);
    poll(&net, 4004);
    assert!(routes().is_empty());
    assert!(!sent_request(&uplink));
    assert!(!sent_request(&downstream));
    transaction.abort().unwrap();
    assert!(stack(&net).unwrap().lock().unwrap().forwarding_enabled.ipv6);
    advertise(&uplink, PEER);
    advertise(&downstream, PEER);
    poll(&net, 4005);
    assert_eq!(routes(), [2]);
}

// 仅解析选项无法覆盖接收接口作用域、独立过期和解析配置观察事件。
#[test]
fn router_advertisement_dns_tracks_scope_withdrawal_and_independent_expiry() {
    use packet_wire::dns_name::UncompressedNames;
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let observer = crate::Event::default();
    crate::inet::observe_dns(&net, observer).unwrap();
    let configuration = prepare_configuration(
        &net, core::iter::empty(), Some(network_protocol::Forwarding { ipv4: true, ipv6: false }),
    ).unwrap();
    configuration.activate().unwrap();
    configuration.confirm().unwrap();

    let global: Ipv6Addr = "2001:db8::53".parse().unwrap();
    let advertise = |servers: &[[u8; 16]], server_lifetime, domain_lifetime, at| {
        let names = UncompressedNames::parse(b"\x06office\x04test\x00\x03lab\x04test\x00").unwrap();
        let mut frame = vec![0; 70 + 8 + 16 * servers.len() + 8 + names.bytes().len().next_multiple_of(8)];
        let after_servers = 78 + 16 * servers.len();
        OptionValue::RecursiveDnsServers { lifetime: server_lifetime, servers }
            .emit(&mut frame[70..after_servers]).unwrap();
        OptionValue::DnsSearchList { lifetime: domain_lifetime, names }
            .emit(&mut frame[after_servers..]).unwrap();
        NdMessage::RouterAdvertisement { hop_limit: 0, flags: 0, lifetime: 1, reachable_time: 0, retransmit_timer: 0 }
            .emit(&mut frame[54..], PEER, ALL).unwrap();
        envelope(&mut frame, PEER, ALL, false, 255);
        enqueue(&device, frame);
        poll(&net, at);
    };
    let capture = |now| {
        let network = stack(&net).unwrap().lock().unwrap();
        super::super::configuration::capture_dns(&network, now).unwrap()
    };
    let take_notification = || {
        let network = stack(&net).unwrap().lock().unwrap();
        let event = network.dns_observer.as_ref().unwrap();
        let ready = event.0.load(core::sync::atomic::Ordering::Relaxed) & native_abi::signals::USER_0 != 0;
        event.signal(native_abi::signals::USER_0, 0).unwrap();
        ready
    };
    take_notification();
    advertise(&[PEER.octets(), global.octets()], 5, 8, 2002);
    assert!(take_notification());
    let learned = capture(2002);
    assert_eq!(learned.servers, ["[fe80::2%2]:53".parse().unwrap(), "[2001:db8::53]:53".parse().unwrap()]);
    assert_eq!(learned.domains, ["office.test", "lab.test"]);
    poll(&net, 3002);
    assert_eq!(capture(3002), learned);
    advertise(&[global.octets(), Ipv6Addr::UNSPECIFIED.octets()], 0, 8, 3502);
    assert_eq!(capture(3502), learned);
    advertise(&[global.octets()], 0, 8, 4002);
    assert!(take_notification());
    assert_eq!(capture(4002).servers, learned.servers[..1]);
    poll(&net, 7002);
    assert!(capture(7002).servers.is_empty());
    assert_eq!(capture(7002).domains, learned.domains);
    poll(&net, 12002);
    assert!(take_notification());
    assert_eq!(capture(12002), router_protocol::dns::LearnedDns::default());
}

// 路由通告横跨实际网卡、DAD、地址投影和 FIB。只检查格式不能证明自动取得的地址可发包。
#[test]
fn router_advertisement_installs_slaac_and_live_paths_with_independent_lifetimes() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let prefix: Ipv6Addr = "2001:db8:1234::".parse().unwrap();
    let automatic = Ipv6Addr::from(u128::from(prefix) | (u128::from(LOCAL) & u128::from(u64::MAX)));
    let capture = |now| {
        let network = stack(&net).unwrap().lock().unwrap();
        super::super::configuration::capture_addresses(&network, 2, now).unwrap()
    };
    let automatic_row = |snapshot: &network_protocol::AddressSnapshot| {
        (0..snapshot.len())
            .filter_map(|index| snapshot.get(index))
            .find(|row| row.address == automatic)
    };
    let remote: Ipv6Addr = "2001:db8:abcd::4".parse().unwrap();
    let on_link: Ipv6Addr = "2001:db8:1234::4".parse().unwrap();
    let all_nodes: Ipv6Addr = "ff02::1".parse().unwrap();
    let advertisement = |hops, router_lifetime, valid_lifetime, preferred_lifetime| {
        let mut frame = vec![0; 110];
        OptionValue::LinkLayerAddress { source: true, address: &MAC }
            .emit(&mut frame[70..78])
            .unwrap();
        OptionValue::Prefix {
            length: 64, flags: 0xc0, valid_lifetime, preferred_lifetime, prefix,
        }.emit(&mut frame[78..]).unwrap();
        NdMessage::RouterAdvertisement {
            hop_limit: 64, flags: 0, lifetime: router_lifetime, reachable_time: 0,
            retransmit_timer: 0,
        }.emit(&mut frame[54..], PEER, all_nodes).unwrap();
        envelope(&mut frame, PEER, all_nodes, false, hops);
        frame
    };
    enqueue(&device, advertisement(255, 6, 10, 4));
    poll(&net, 2002);
    assert!(automatic_row(&capture(2002)).is_none());

    let configuration = prepare_configuration(
        &net,
        core::iter::empty(),
        Some(network_protocol::Forwarding { ipv4: true, ipv6: false }),
    ).unwrap();
    configuration.activate().unwrap();
    configuration.confirm().unwrap();
    poll(&net, 2003);
    poll(&net, 3003);
    let output = frames(&device);
    assert!(output.iter().any(|frame| {
        let (ip, icmp) = parse(frame);
        ip.hop_limit() == 255
            && NdPacket::parse(icmp).is_ok_and(|nd| nd.message() == NdMessage::RouterSolicitation)
    }));
    enqueue(&device, advertisement(254, 6, 10, 4));
    poll(&net, 3004);
    assert!(automatic_row(&capture(3004)).is_none());

    enqueue(&device, advertisement(255, 6, 10, 4));
    poll(&net, 3005);
    let initial = capture(3005);
    let tentative = automatic_row(&initial).unwrap();
    assert_eq!(tentative.state, State::Tentative);
    assert_eq!(tentative.origin, network_protocol::AddressOrigin::Slaac);
    assert!(!tentative.usable);
    assert_eq!((tentative.preferred_lifetime, tentative.valid_lifetime), (4, 10));
    poll(&net, 4005);
    poll(&net, 5005);
    assert_eq!(automatic_row(&capture(5005)).unwrap().state, State::Preferred);
    frames(&device);

    let socket = socket6(&net);
    socket.send_to(b"automatic", Some(InetAddress::new(remote.into(), 44333)), &[]).unwrap();
    poll(&net, 5006);
    let output = frames(&device);
    let data = output.iter().find_map(|frame| {
        let ethernet = EthernetFrame::parse(frame).ok()?;
        let ip = Ipv6Packet::parse(ethernet.payload()).ok()?;
        (ip.upper_protocol() == 17).then_some((ethernet.destination(), ip))
    }).unwrap();
    assert_eq!(data.0, MAC);
    assert_eq!(data.1.source(), automatic);
    assert_eq!(data.1.destination(), remote);

    let route = |destination: Ipv6Addr, at: i64| {
        stack(&net).unwrap().lock().unwrap().ip_output(
            Ipv6Addr::UNSPECIFIED.into(), InetAddress::new(destination.into(), 44333),
            (None, 0), Instant::from_millis(at),
        ).map(|route| route.neighbor)
    };
    assert_eq!(route(on_link, 5006), Ok(on_link.into()));
    assert_eq!(route(remote, 5006), Ok(PEER.into()));
    let snapshot = super::super::configuration::route_snapshot::<Ipv6Addr>(&net).unwrap();
    assert!(snapshot.iter().any(|row| row.route.prefix_length == 0 && row.route.protocol == 9));

    let manual = [definition(automatic, 80, false)];
    let transaction = prepare_configuration(&net, [plan(2, &manual)].into_iter(), None).unwrap();
    transaction.activate().unwrap();
    let installed = automatic_row(&capture(5006)).unwrap();
    assert_eq!(installed.origin, network_protocol::AddressOrigin::Configured);
    assert_eq!(installed.prefix_length, 80);
    assert!(installed.usable);
    transaction.abort().unwrap();
    assert_eq!(automatic_row(&capture(5006)), Some(network_protocol::InterfaceAddress {
        state: State::Preferred,
        usable: true,
        preferred_lifetime: 2,
        valid_lifetime: 8,
        ..tentative
    }));

    let deprecated = automatic_row(&capture(7005)).unwrap();
    assert_eq!(deprecated.state, State::Deprecated);
    assert_eq!((deprecated.preferred_lifetime, deprecated.valid_lifetime), (0, 6));
    assert_eq!(automatic_row(&initial), Some(tentative));
    poll(&net, 7005);
    let deprecated = automatic_row(&capture(7005)).unwrap();
    assert_eq!(deprecated.state, State::Deprecated);
    assert!(deprecated.usable);
    poll(&net, 9005);
    assert_eq!(route(remote, 9005), Err(SocketError::NetworkUnreachable));
    assert_eq!(route(on_link, 9005), Ok(on_link.into()));
    assert!(automatic_row(&capture(13005)).is_none());
    poll(&net, 13005);
    assert!(automatic_row(&capture(13005)).is_none());
    assert_eq!(route(on_link, 13005), Err(SocketError::NetworkUnreachable));

    // 旧地址的短有效期不会被未经认证的零值通告提前撤销。
    enqueue(&device, advertisement(255, 20, 10000, 10000));
    poll(&net, 13006);
    poll(&net, 14006);
    poll(&net, 15006);
    enqueue(&device, advertisement(255, 0, 0, 0));
    poll(&net, 15007);
    let protected = automatic_row(&capture(15007)).unwrap();
    assert_eq!(protected.state, State::Deprecated);
    assert!(protected.usable);
    assert_eq!(protected.valid_lifetime, 7200);
    assert_eq!(route(on_link, 15007), Err(SocketError::NetworkUnreachable));
    poll(&net, 7_215_007);
    assert!(automatic_row(&capture(7_215_007)).is_none());
    socket.close_description();
}

// A route option must affect frames, then leave other routers' paths intact when withdrawn.
#[test]
fn route_information_selects_and_withdraws_paths_independently_of_default_router() {
    use packet_wire::ndisc::options::RoutePreference;

    let _serial = SERIAL.lock().unwrap();
    initialize();
    let (net, device) = domain();
    ready(&net, &device);
    let addresses = [definition(GLOBAL, 64, true)];
    let configuration = prepare_configuration(
        &net,
        [plan(2, &addresses)].into_iter(),
        Some(network_protocol::Forwarding { ipv4: true, ipv6: false }),
    ).unwrap();
    configuration.activate().unwrap();
    configuration.confirm().unwrap();
    advance_configuration(&net, 2002);
    let alternate: Ipv6Addr = "fe80::3".parse().unwrap();
    let alternate_mac = [2, 0, 0, 0, 0, 3];
    let prefix: Ipv6Addr = "2001:db8:abcd::".parse().unwrap();
    let destination: Ipv6Addr = "2001:db8:abcd:1::4".parse().unwrap();
    let other: Ipv6Addr = "2001:db8:ffff::4".parse().unwrap();
    let advertise = |source, mac: &[u8; 6], lifetime, routes: &[(u8, u32, RoutePreference)]| {
        let mut frame = vec![0; 78];
        OptionValue::LinkLayerAddress { source: true, address: mac }
            .emit(&mut frame[70..78])
            .unwrap();
        for &(length, lifetime, preference) in routes {
            let offset = frame.len();
            let size = if length == 0 { 8 } else { 16 };
            frame.resize(offset + size, 0);
            OptionValue::RouteInformation { length, lifetime, preference, prefix }
                .emit(&mut frame[offset..])
                .unwrap();
        }
        NdMessage::RouterAdvertisement {
            hop_limit: 64, flags: 0, lifetime, reachable_time: 0, retransmit_timer: 0,
        }.emit(&mut frame[54..], source, ALL).unwrap();
        envelope(&mut frame, source, ALL, false, 255);
        enqueue(&device, frame);
    };
    let socket = socket6(&net);
    let send = |destination: Ipv6Addr, at| {
        frames(&device);
        socket.send_to(b"route", Some(InetAddress::new(destination.into(), 44333)), &[]).unwrap();
        poll(&net, at);
        frames(&device).iter().find_map(|frame| {
            let ethernet = EthernetFrame::parse(frame).ok()?;
            let ip = Ipv6Packet::parse(ethernet.payload()).ok()?;
            (ip.upper_protocol() == 17 && ip.destination() == destination)
                .then_some(ethernet.destination())
        }).unwrap_or_else(|| panic!("no UDP frame to {destination} at {at}"))
    };

    advertise(PEER, &MAC, 60, &[(48, 30, RoutePreference::Low)]);
    advertise(alternate, &alternate_mac, 0, &[
        (0, 3, RoutePreference::High),
        (48, 6, RoutePreference::High),
    ]);
    poll(&net, 5000);
    assert_eq!(send(destination, 5001), alternate_mac);
    assert_eq!(send(other, 5002), alternate_mac);
    let frozen = super::super::configuration::route_snapshot::<Ipv6Addr>(&net).unwrap();
    let has_specific = |snapshot: &network_protocol::RoutingSnapshot<Ipv6Addr>| {
        snapshot.iter().any(|row| {
            row.route.protocol == 9 && row.route.destination == prefix
                && row.route.prefix_length == 48 && row.route.metric == 1024
                && row.single_path().is_some_and(|path| path.gateway == Some(alternate))
        })
    };
    assert!(has_specific(&frozen));
    poll(&net, 8000);
    assert_eq!(send(other, 8001), MAC);
    assert_eq!(send(destination, 8002), alternate_mac);
    poll(&net, 11000);
    assert_eq!(send(destination, 11001), MAC);
    assert!(has_specific(&frozen));

    advertise(alternate, &alternate_mac, 30, &[
        (0, 0, RoutePreference::High),
        (48, u32::MAX, RoutePreference::High),
    ]);
    poll(&net, 11002);
    assert_eq!(send(other, 11003), MAC);
    assert_eq!(send(destination, 11004), alternate_mac);
    advertise(alternate, &alternate_mac, 0, &[(48, 0, RoutePreference::High)]);
    poll(&net, 11005);
    assert_eq!(send(destination, 11006), MAC);
    let current = super::super::configuration::route_snapshot::<Ipv6Addr>(&net).unwrap();
    assert!(!has_specific(&current));

    advertise(alternate, &alternate_mac, 30, &[
        (0, 30, RoutePreference::High),
        (48, 30, RoutePreference::High),
    ]);
    poll(&net, 11007);
    let mut demotion = vec![0; 78];
    NdMessage::NeighborAdvertisement { flags: 0x4000_0000, target: alternate }
        .emit(&mut demotion[54..], alternate, LOCAL)
        .unwrap();
    envelope(&mut demotion, alternate, LOCAL, false, 255);
    enqueue(&device, demotion);
    poll(&net, 11008);
    assert_eq!(send(destination, 11009), MAC);
    assert_eq!(send(other, 11010), MAC);

    // A missing reply must move actual traffic to a shorter route while retaining the preferred RIO.
    let preferred: Ipv6Addr = "fe80::4".parse().unwrap();
    let preferred_mac = [2, 0, 0, 0, 0, 4];
    let answer = |router, time| {
        let mut frame = vec![0; 78];
        NdMessage::NeighborAdvertisement { flags: 0xe000_0000, target: router }
            .emit(&mut frame[54..], router, LOCAL).unwrap();
        envelope(&mut frame, router, LOCAL, false, 255);
        enqueue(&device, frame);
        poll(&net, time);
    };
    advertise(PEER, &MAC, 120, &[(48, 0, RoutePreference::Low)]);
    advertise(preferred, &preferred_mac, 0, &[(48, 120, RoutePreference::High)]);
    answer(PEER, 12000);
    assert_eq!(send(destination, 12001), preferred_mac);
    for time in [17001, 18001, 19001, 20001] {
        poll(&net, time);
    }
    let neighbor = |address| {
        let snapshot = super::super::configuration::neighbor_snapshot(&net, 2).unwrap();
        (0..snapshot.len()).filter_map(|index| snapshot.get(index))
            .find(|row| row.address == address).unwrap()
    };
    let failed = neighbor(preferred);
    assert_eq!(failed.state, network_protocol::NeighborState::Failed);
    assert_eq!(failed.hardware_address, None);
    assert_eq!(send(destination, 20002), MAC);
    let reprobes = || {
        frames(&device).iter().filter(|frame| {
            let (_, icmp) = parse(frame);
            NdPacket::parse(icmp).is_ok_and(|nd| nd.message() == NdMessage::NeighborSolicitation { target: preferred })
        }).count()
    };
    poll(&net, 20003);
    assert_eq!(reprobes(), 1);
    assert_eq!(send(destination, 20004), MAC);
    poll(&net, 20005);
    assert_eq!(reprobes(), 0);
    answer(PEER, 20006);
    poll(&net, 80003);
    assert_eq!(reprobes(), 0);
    assert_eq!(send(destination, 80004), MAC);
    poll(&net, 80005);
    assert_eq!(reprobes(), 1);

    answer(preferred, 80006);
    assert_eq!(neighbor(preferred).state, network_protocol::NeighborState::Reachable);
    assert_eq!(send(destination, 80007), preferred_mac);
    socket.close_description();
}
