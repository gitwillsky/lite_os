//! 直接编码 Linux 生产快照投影，验证可变 rtnexthop framing，不构造替代 codec。
extern crate alloc;
mod net {
    pub(crate) use network_protocol::{InterfaceSnapshot, RouteKind, RoutingSnapshot};
}
#[path = "../../../tools/source-view/linuxd/net/rtnetlink/codec/codec/codec.rs"]
#[expect(
    dead_code,
    reason = "本夹具只覆盖路由消息；link/address 与请求分派由来宾 ABI 用例覆盖"
)]
mod codec;
use core::net::{Ipv4Addr, Ipv6Addr};
use network_protocol::routes::{NextHopConfiguration, NextHopMember, RouteAttributes};
use network_protocol::{RouteKind, RouteMetrics, RoutingSnapshot};

#[test]
fn rule_dump_preserves_priority_source_ingress_mark_and_large_table_in_linux_uapi() {
    use network_protocol::{
        InetFamily,
        routes::{RuleEntry, RuleSnapshot},
    };
    use router_protocol::policy_routing::Rule;

    let mut request = [0; 12];
    request[0] = 10;
    assert_eq!(codec::rule_query(&request), Ok(Some(InetFamily::Ipv6)));
    assert_eq!(
        codec::rule_query(&[2, 0x91, 0xb2, 0x7f]),
        Ok(Some(InetFamily::Ipv4))
    );
    assert_eq!(codec::rule_query(&[0]), Ok(None));
    request[2] = 128;
    assert_eq!(codec::rule_query(&request), Err(22));
    assert_eq!(codec::rule_query(&[17]), Err(97));

    let source: Ipv6Addr = "2001:db8:1::".parse().unwrap();
    let mut name = [0; 16];
    name[..4].copy_from_slice(b"eth1");
    let entry = RuleEntry {
        rule: Rule {
            priority: 17,
            source: source.into(),
            prefix_length: 64,
            ingress: 3,
            table: 1000,
            mark: 0x8000_0000,
            mark_mask: 0x8000_0000,
        },
        ingress_name: name,
    };
    let captured = RuleSnapshot::capture([entry].into_iter()).unwrap();
    let snapshot = RuleSnapshot::parse(captured.as_bytes().to_vec()).unwrap();
    let (message, length) = codec::rule(snapshot.get(0).unwrap(), 8, 123);
    let header = codec::parse_header(&message[..length]).unwrap();
    assert_eq!(
        (header.kind, header.flags, header.sequence),
        (codec::RTM_NEWRULE, codec::NLM_F_MULTI, 8)
    );
    let payload = &message[codec::HEADER_LENGTH..length];
    assert_eq!(&payload[..12], &[10, 0, 64, 0, 252, 0, 0, 1, 0, 0, 0, 0]);
    assert_eq!(
        codec::find_attribute(payload, 12, 2),
        Some(source.octets().as_slice())
    );
    assert_eq!(
        codec::find_attribute(payload, 12, 3),
        Some(b"eth1\0".as_slice())
    );
    assert_eq!(
        codec::find_attribute(payload, 12, 6),
        Some(17u32.to_ne_bytes().as_slice())
    );
    assert_eq!(
        codec::find_attribute(payload, 12, 15),
        Some(1000u32.to_ne_bytes().as_slice())
    );

    assert_eq!(codec::find_attribute(payload, 12, 10), Some(entry.rule.mark.to_ne_bytes().as_slice()));
    assert_eq!(codec::find_attribute(payload, 12, 16), Some(entry.rule.mark_mask.to_ne_bytes().as_slice()));

    // 原生快照禁止把隐藏的接口名后缀或重复优先级交给 Linux。
    let mut invalid = captured.as_bytes().to_vec();
    *invalid.last_mut().unwrap() = b'x';
    assert!(RuleSnapshot::parse(invalid).is_err());
    assert!(RuleSnapshot::capture([entry, entry].into_iter()).is_err());
}

#[test]
fn route_get_encodes_a_single_resolved_path_with_cross_family_gateway() {
    use network_protocol::routes::{RouteDirection, RoutePath};
    let mut request = vec![2, 32, 0, 0, 0, 0, 0, 0];
    request.extend_from_slice(&0x1000u32.to_ne_bytes()); // iproute2 RTM_F_LOOKUP_TABLE
    request.extend_from_slice(&8u16.to_ne_bytes());
    request.extend_from_slice(&1u16.to_ne_bytes());
    request.extend_from_slice(&[203, 0, 113, 9]);
    let mark = 0x8000_0042u32;
    let mut attribute = [0; 8];
    attribute[..2].copy_from_slice(&8u16.to_ne_bytes());
    attribute[2..4].copy_from_slice(&16u16.to_ne_bytes());
    attribute[4..].copy_from_slice(&mark.to_ne_bytes());
    request.extend_from_slice(&attribute);
    let query = codec::route_query(&request).unwrap();
    assert_eq!(query.mark, mark);
    assert_eq!(query.destination, Ipv4Addr::new(203, 0, 113, 9));
    assert!(query.source.is_unspecified());
    assert_eq!(query.direction, RouteDirection::Output(None));
    let gateway: Ipv6Addr = "fe80::2".parse().unwrap();
    let bytes = codec::route_path(
        query,
        RoutePath {
            table: 254,
            interface_index: 3,
            kind: RouteKind::Unicast,
            source: Ipv4Addr::new(10, 40, 0, 15).into(),
            gateway: Some(gateway.into()),
        },
        12,
        42,
    )
    .unwrap();
    let header = codec::parse_header(&bytes).unwrap();
    assert_eq!(
        (header.kind, header.flags, header.sequence),
        (codec::RTM_NEWROUTE, 0, 12)
    );
    let payload = &bytes[codec::HEADER_LENGTH..];
    assert_eq!(
        codec::find_attribute(payload, 12, 4),
        Some(3u32.to_ne_bytes().as_slice())
    );
    assert_eq!(
        codec::find_attribute(payload, 12, 7),
        Some([10, 40, 0, 15].as_slice())
    );
    assert!(codec::find_attribute(payload, 12, 5).is_none());
    let via = codec::find_attribute(payload, 12, 18).unwrap();
    assert_eq!(&via[..2], &10u16.to_ne_bytes());
    assert_eq!(&via[2..], &gateway.octets());

    assert_eq!(codec::find_attribute(payload, 12, 16), Some(mark.to_ne_bytes().as_slice()));

    // 截断或重复的标记不得降级成忽略条件的查询。
    assert_eq!(codec::route_query(&request[..request.len() - 1]), Err(22));
    request.extend_from_slice(&attribute);
    assert_eq!(codec::route_query(&request), Err(22));

    let destination: Ipv6Addr = "2001:db8::9".parse().unwrap();
    let source: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
    let mut request = vec![10, 128, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    for (kind, address) in [(1u16, destination), (2, source)] {
        request.extend_from_slice(&20u16.to_ne_bytes());
        request.extend_from_slice(&kind.to_ne_bytes());
        request.extend_from_slice(&address.octets());
    }
    request.extend_from_slice(&8u16.to_ne_bytes());
    request.extend_from_slice(&4u16.to_ne_bytes());
    request.extend_from_slice(&3u32.to_ne_bytes());
    let query = codec::route_query(&request).unwrap();
    assert_eq!(
        (query.source, query.direction),
        (source.into(), RouteDirection::Output(Some(3)))
    );
    let bytes = codec::route_path(
        query,
        RoutePath {
            table: 254,
            interface_index: 3,
            kind: RouteKind::Unicast,
            source: source.into(),
            gateway: Some(gateway.into()),
        },
        13,
        42,
    )
    .unwrap();
    let payload = &bytes[codec::HEADER_LENGTH..];
    assert_eq!(&payload[..3], &[10, 128, 128]);
    assert_eq!(
        codec::find_attribute(payload, 12, 2),
        Some(source.octets().as_slice())
    );
    assert_eq!(
        codec::find_attribute(payload, 12, 5),
        Some(gateway.octets().as_slice())
    );
    assert!(codec::find_attribute(payload, 12, 18).is_none());

    let iif = [8u16.to_ne_bytes(), 3u16.to_ne_bytes()].concat();
    request.extend_from_slice(&iif);
    request.extend_from_slice(&2u32.to_ne_bytes());
    let query = codec::route_query(&request).unwrap();
    assert_eq!(query.direction, RouteDirection::Input(2));
    let response = codec::route_path(
        query,
        RoutePath {
            table: 1000,
            interface_index: 4,
            kind: RouteKind::Unicast,
            source: source.into(),
            gateway: Some(gateway.into()),
        },
        14,
        42,
    )
    .unwrap();
    let payload = &response[codec::HEADER_LENGTH..];
    for (attribute, value) in [(3, 2u32), (4, 4), (15, 1000)] {
        assert_eq!(
            codec::find_attribute(payload, 12, attribute),
            Some(value.to_ne_bytes().as_slice())
        );
    }
    assert!(codec::find_attribute(payload, 12, 7).is_none());
    request.extend_from_slice(&iif);
    request.extend_from_slice(&2u32.to_ne_bytes());
    assert_eq!(codec::route_query(&request), Err(22));
}

fn snapshot<A: codec::RouteAddress>(
    members: &[NextHopMember<A>],
    count: usize,
) -> RoutingSnapshot<A> {
    let route = RouteAttributes {
        destination: A::UNSPECIFIED,
        prefix_length: 0,
        table: 254,
        metric: 3,
        scope: 0,
        protocol: 3,
        kind: RouteKind::Unicast,
        preferred_source: None,
        metrics: RouteMetrics::default(),
    };
    let bytes = RoutingSnapshot::encode(
        [(2, [0; 16]), (3, [0; 16])].into_iter(),
        [NextHopConfiguration::new(1, members)].into_iter(),
        (0..count).map(|_| (route, Some(0), false)),
    )
    .unwrap();
    RoutingSnapshot::parse(bytes).unwrap()
}
#[test]
fn production_dump_preserves_all_paths_and_enforces_exact_uapi_bounds() {
    let mut members: [NextHopMember<Ipv4Addr>; 2] = [
        NextHopMember {
            interface_index: 2,
            gateway: Some(Ipv4Addr::new(192, 0, 2, 1).into()),
            on_link: true,
            weight: 300,
        },
        NextHopMember {
            interface_index: 3,
            gateway: None,
            on_link: false,
            weight: 100,
        },
    ];
    let capture = snapshot(&members, 2);
    assert_eq!(capture.group_count(), 1);
    let dump = codec::RouteDump::new(capture).unwrap();
    let mut out = Vec::with_capacity(dump.maximum_message_size());
    assert!(dump.append(0, 47, 9, 1, &mut out));
    let header = codec::parse_header(&out).unwrap();
    assert_eq!(header.length, out.len());
    assert_eq!(header.sequence, 47);
    let payload = &out[codec::HEADER_LENGTH..];
    assert!(codec::find_attribute(payload, 12, 4).is_none()); // 没有伪造一个顶层 OIF。
    assert!(codec::find_attribute(payload, 12, 5).is_none());
    let paths = codec::find_attribute(payload, 12, 9).unwrap();
    assert_eq!(paths.len(), 24);
    assert_eq!(u16::from_ne_bytes(paths[..2].try_into().unwrap()), 16);
    assert_eq!(&paths[2..4], &[4, 2]); // on-link；300:100 精确约为 3:1，hops=weight-1。
    assert_eq!(i32::from_ne_bytes(paths[4..8].try_into().unwrap()), 2);
    assert_eq!(
        codec::find_attribute(paths, 8, 5),
        Some([192, 0, 2, 1].as_slice())
    );
    assert_eq!(u16::from_ne_bytes(paths[16..18].try_into().unwrap()), 8);
    assert_eq!(&paths[18..20], &[0, 0]);
    assert_eq!(i32::from_ne_bytes(paths[20..24].try_into().unwrap()), 3);
    let prefix = out.clone();
    assert!(!dump.append(1, 47, 9, out.len(), &mut out));
    assert_eq!(
        out, prefix,
        "insufficient window cannot append half a route"
    );
    out.clear();
    assert!(dump.append(1, 47, 9, 1, &mut out));
    assert_eq!(out, prefix);

    members[0].weight = 257;
    members[1].weight = 1;
    assert!(matches!(
        codec::RouteDump::new(snapshot(&members, 1)),
        Err(-75)
    ));
    members[0].weight = 256;
    let dump = codec::RouteDump::new(snapshot(&members, 1)).unwrap();
    out.clear();
    assert!(dump.append(0, 0, 0, 1, &mut out));
    assert_eq!(codec::find_attribute(&out[16..], 12, 9).unwrap()[3], 255);

    // 大于旧 1024-byte 栈缓冲、16-KiB 数据报窗口的合法首消息完整编码，不截断成员。
    members[0].weight = 1;
    let large = vec![members[0]; 1100];
    let dump = codec::RouteDump::new(snapshot(&large, 1)).unwrap();
    out = Vec::with_capacity(dump.maximum_message_size());
    assert!(dump.append(0, 0, 0, 16384, &mut out));
    assert!(out.len() > 16384);
    assert_eq!(
        codec::find_attribute(&out[16..], 12, 9).unwrap().len(),
        1100 * 16
    );
    assert!(matches!(
        codec::RouteDump::new(snapshot(&vec![members[0]; 4096], 1)),
        Err(-90)
    ));
    // IPv6 使用同一生产 codec；4-byte 地址断言无法捕获 16-byte gateway 的成员边界和长度上限。
    let six: [NextHopMember<Ipv6Addr>; 2] = [
        NextHopMember {
            interface_index: 2,
            gateway: Some("fe80::1234:5678:9abc:def0".parse::<Ipv6Addr>().unwrap()),
            on_link: true,
            weight: 300,
        },
        NextHopMember {
            interface_index: 3,
            gateway: None,
            on_link: false,
            weight: 100,
        },
    ];
    let dump6 = codec::RouteDump::new(snapshot(&six, 2)).unwrap();
    let mut output6 = Vec::with_capacity(dump6.maximum_message_size());
    assert!(dump6.append(0, 47, 9, 1, &mut output6));
    assert_eq!(output6[16], 10);
    let paths6 = codec::find_attribute(&output6[16..], 12, 9).unwrap();
    assert_eq!(paths6.len(), 36);
    assert_eq!(u16::from_ne_bytes(paths6[..2].try_into().unwrap()), 28);
    assert_eq!(&paths6[2..4], &[4, 2]);
    assert_eq!(
        codec::find_attribute(paths6, 8, 5),
        Some(six[0].gateway.unwrap().octets().as_slice())
    );
    assert_eq!(u16::from_ne_bytes(paths6[28..30].try_into().unwrap()), 8);
    let frozen6 = output6.clone();
    assert!(!dump6.append(1, 47, 9, output6.len(), &mut output6));
    assert_eq!(output6, frozen6);
    let huge6 = vec![six[0]; 2341]; // 4 + 2341×28 超出 u16；不能沿用 IPv4 的成员数上限。
    assert!(matches!(
        codec::RouteDump::new(snapshot(&huge6, 1)),
        Err(-90)
    ));
    let single6 = codec::RouteDump::new(snapshot(&six[..1], 1)).unwrap();
    output6.clear();
    assert!(single6.append(0, 47, 9, 1, &mut output6));
    assert_eq!(
        codec::find_attribute(&output6[16..], 12, 4),
        Some(2u32.to_ne_bytes().as_slice())
    );
    assert_eq!(
        codec::find_attribute(&output6[16..], 12, 5),
        Some(six[0].gateway.unwrap().octets().as_slice())
    );
    members[0].gateway = six[0].gateway.map(Into::into);
    members[1].gateway = Some(Ipv4Addr::new(192, 0, 2, 1).into());
    let mut via = 10u16.to_ne_bytes().to_vec();
    via.extend_from_slice(&six[0].gateway.unwrap().octets());
    let mixed = codec::RouteDump::new(snapshot(&members, 1)).unwrap();
    out.clear();
    assert!(mixed.append(0, 0, 0, 1, &mut out));
    let paths = codec::find_attribute(&out[16..], 12, 9).unwrap();
    assert_eq!(paths.len(), 48);
    assert_eq!(u16::from_ne_bytes(paths[..2].try_into().unwrap()), 32);
    assert_eq!(
        codec::find_attribute(&paths[..32], 8, 18),
        Some(via.as_slice())
    );
    assert_eq!(
        codec::find_attribute(&paths[32..], 8, 5),
        Some([192, 0, 2, 1].as_slice())
    );
    let single = codec::RouteDump::new(snapshot(&members[..1], 1)).unwrap();
    out.clear();
    assert!(single.append(0, 0, 0, 1, &mut out));
    assert!(codec::find_attribute(&out[16..], 12, 5).is_none());
    assert_eq!(
        codec::find_attribute(&out[16..], 12, 18),
        Some(via.as_slice())
    );
    for (kind, value) in [
        (RouteKind::Blackhole, 6),
        (RouteKind::Prohibit, 8),
        (RouteKind::Unreachable, 7),
    ] {
        let attributes = RouteAttributes {
            destination: Ipv4Addr::new(192, 0, 2, 0),
            prefix_length: 24,
            table: 254,
            metric: 0,
            scope: 0,
            protocol: 3,
            kind,
            preferred_source: None,
            metrics: RouteMetrics::default(),
        };
        let bytes = RoutingSnapshot::encode(
            [].into_iter(),
            [].into_iter(),
            [(attributes, None, false)].into_iter(),
        )
        .unwrap();
        let dump =
            codec::RouteDump::new(RoutingSnapshot::<Ipv4Addr>::parse(bytes).unwrap()).unwrap();
        out = Vec::with_capacity(dump.maximum_message_size());
        assert!(dump.append(0, 0, 0, 1, &mut out));
        assert_eq!(out[codec::HEADER_LENGTH + 7], value);
        assert!(codec::find_attribute(&out[codec::HEADER_LENGTH..], 12, 4).is_none());
        assert!(codec::find_attribute(&out[codec::HEADER_LENGTH..], 12, 5).is_none());
    }
}

// IPv4 dump 用例不能捕获 IPv6 DAD 状态误报或 ifaddrmsg 家族/地址属性的投影错误。
#[test]
fn automatic_ipv6_address_keeps_native_dad_state_in_standard_netlink_attributes() {
    use network_protocol::{AddressState, InterfaceAddress};
    let address = "fe80::ff:fe00:7".parse::<core::net::Ipv6Addr>().unwrap();
    for (state, flags) in [
        (AddressState::Tentative, 0xc0u32),
        (AddressState::Preferred, 0x80),
        (AddressState::Duplicate, 0xc8),
    ] {
        let row = InterfaceAddress {
            interface_index: 7,
            name: [0; 16],
            address: address.into(),
            prefix_length: 64,
            state,
            usable: state == AddressState::Preferred,
            origin: network_protocol::AddressOrigin::Interface,
            on_link: true,
            broadcast: None,
            preferred_lifetime: u32::MAX,
            valid_lifetime: u32::MAX,
        };
        let (bytes, length) = codec::address(row, 41, 9);
        let header = codec::parse_header(&bytes[..length]).unwrap();
        assert_eq!((header.kind, header.sequence), (codec::RTM_NEWADDR, 41));
        let payload = &bytes[16..length];
        assert_eq!(&payload[..4], &[10, 64, flags as u8, 253]);
        assert_eq!(u32::from_ne_bytes(payload[4..8].try_into().unwrap()), 7);
        assert_eq!(
            codec::find_attribute(payload, 8, 1),
            Some(address.octets().as_slice())
        );
        assert_eq!(
            codec::find_attribute(payload, 8, 8),
            Some(flags.to_ne_bytes().as_slice())
        );
        assert!(codec::find_attribute(payload, 8, 2).is_none());
        assert!(codec::find_attribute(payload, 8, 3).is_none());
    }
    let loopback = InterfaceAddress {
        interface_index: 1,
        name: [0; 16],
        address: core::net::Ipv6Addr::LOCALHOST.into(),
        prefix_length: 128,
        state: AddressState::Preferred,
        usable: true,
        origin: network_protocol::AddressOrigin::Interface,
        on_link: true,
        broadcast: None,
        preferred_lifetime: u32::MAX,
        valid_lifetime: u32::MAX,
    };
    let (bytes, _) = codec::address(loopback, 41, 9);
    assert_eq!(&bytes[16..20], &[10, 128, 0x80, 254]);
    let static_address = InterfaceAddress {
        interface_index: 7,
        address: "2001:db8:7::1"
            .parse::<core::net::Ipv6Addr>()
            .unwrap()
            .into(),
        prefix_length: 80,
        on_link: false,
        ..loopback
    };
    let (bytes, length) = codec::address(static_address, 41, 9);
    let payload = &bytes[16..length];
    assert_eq!(&payload[..4], &[10, 80, 0x80, 0]);
    assert_eq!(
        codec::find_attribute(payload, 8, 8),
        Some(0x280u32.to_ne_bytes().as_slice())
    );
    for (state, preferred, flags) in [
        (AddressState::Preferred, 7u32, 0x200u32),
        (AddressState::Deprecated, 0, 0x220),
    ] {
        let automatic = InterfaceAddress {
            state,
            origin: network_protocol::AddressOrigin::Slaac,
            prefix_length: 64,
            preferred_lifetime: preferred,
            valid_lifetime: 20,
            ..static_address
        };
        let (bytes, length) = codec::address(automatic, 41, 9);
        let payload = &bytes[16..length];
        let cache = codec::find_attribute(payload, 8, 6).unwrap();
        assert_eq!(cache.len(), 16);
        assert_eq!(&cache[..4], &preferred.to_ne_bytes());
        assert_eq!(&cache[4..8], &20u32.to_ne_bytes());
        assert_eq!(
            codec::find_attribute(payload, 8, 8),
            Some(flags.to_ne_bytes().as_slice())
        );
    }
    let native = network_protocol::AddressSnapshot::capture(
        [InterfaceAddress {
            interface_index: u32::MAX,
            ..loopback
        }]
        .into_iter(),
    )
    .unwrap();
    assert_eq!(codec::validate_addresses(&native, 0), Err(75));
    assert_eq!(codec::validate_addresses(&native, 2), Ok(()));
}

// 原 FIB/address 用例无法证明 ndmsg 状态位、请求属性或不可用 metadata 的正确省略。
#[test]
fn neighbor_projection_matches_ndmsg_and_never_fabricates_age_or_inactive_probe_counts() {
    use network_protocol::{Neighbor, NeighborSnapshot, NeighborState};
    let mut row = Neighbor {
        interface_index: 2,
        address: "fe80::2".parse().unwrap(),
        hardware_address: None,
        state: NeighborState::Incomplete,
        is_router: false,
        probes: 2,
    };
    for (state, bits) in [
        (NeighborState::Incomplete, 1),
        (NeighborState::Reachable, 2),
        (NeighborState::Stale, 4),
        (NeighborState::Delay, 8),
        (NeighborState::Probe, 16),
        (NeighborState::Failed, 32),
    ] {
        row.state = state;
        row.hardware_address =
            (!matches!(state, NeighborState::Incomplete | NeighborState::Failed))
                .then_some([2, 0, 0, 0, 0, 2]);
        row.probes = if matches!(state, NeighborState::Incomplete | NeighborState::Probe) {
            2
        } else {
            0
        };
        row.is_router = true;
        let snapshot = NeighborSnapshot::capture([row].into_iter()).unwrap();
        codec::validate_neighbors(&snapshot).unwrap();
        let (bytes, length) =
            codec::neighbor(row, codec::NLM_F_MULTI | codec::NLM_F_DUMP_FILTERED, 77, 9);
        let header = codec::parse_header(&bytes[..length]).unwrap();
        assert_eq!((header.kind, header.flags, header.sequence), (28, 0x22, 77));
        let payload = &bytes[16..length];
        assert_eq!(&payload[..4], &[10, 0, 0, 0]);
        assert_eq!(u32::from_ne_bytes(payload[4..8].try_into().unwrap()), 2);
        assert_eq!(u16::from_ne_bytes(payload[8..10].try_into().unwrap()), bits);
        assert_eq!(&payload[10..12], &[0x80, 1]);
        assert_eq!(
            codec::find_attribute(payload, 12, 1),
            Some(
                "fe80::2"
                    .parse::<core::net::Ipv6Addr>()
                    .unwrap()
                    .octets()
                    .as_slice()
            )
        );
        assert_eq!(
            codec::find_attribute(payload, 12, 2),
            row.hardware_address.as_ref().map(|mac| mac.as_slice())
        );
        assert_eq!(
            codec::find_attribute(payload, 12, 4).is_some(),
            matches!(state, NeighborState::Incomplete | NeighborState::Probe)
        );
        assert!(codec::find_attribute(payload, 12, 3).is_none());
    }
    row.address = "192.0.2.7".parse().unwrap();
    row.is_router = false;
    let (bytes, length) = codec::neighbor(row, 0, 77, 9);
    let payload = &bytes[16..length];
    assert_eq!(&payload[..4], &[2, 0, 0, 0]);
    assert_eq!(
        codec::find_attribute(payload, 12, 1),
        Some([192, 0, 2, 7].as_slice())
    );
    row.interface_index = u32::MAX;
    assert_eq!(
        codec::validate_neighbors(&NeighborSnapshot::capture([row].into_iter()).unwrap()),
        Err(75)
    );
}

#[test]
fn neighbor_queries_use_standard_ifindex_filter_and_strict_single_lookup_attributes() {
    let address: core::net::Ipv6Addr = "fe80::2".parse().unwrap();
    let mut request = vec![0; 12];
    request[0] = 10;
    request[4..8].copy_from_slice(&2i32.to_ne_bytes());
    assert_eq!(codec::neighbor_dump(&request), Ok((0, false))); // ndm_ifindex 不是 dump 的过滤属性。
    request.extend_from_slice(&20u16.to_ne_bytes());
    request.extend_from_slice(&1u16.to_ne_bytes());
    request.extend_from_slice(&address.octets());
    assert_eq!(
        codec::neighbor_get(&request),
        Ok((2, address.into(), false))
    );
    request[10] = 8;
    assert_eq!(codec::neighbor_get(&request), Ok((2, address.into(), true)));
    request[10] = 1;
    assert_eq!(codec::neighbor_get(&request), Err(22));
    request[10] = 0;
    request[0] = 0;
    assert_eq!(codec::neighbor_get(&request), Err(97));
    request[0] = 10;
    request[14..16].copy_from_slice(&2u16.to_ne_bytes());
    assert_eq!(codec::neighbor_get(&request), Err(22));
    request.truncate(12);
    request[0] = 2;
    request.extend_from_slice(&8u16.to_ne_bytes());
    request.extend_from_slice(&1u16.to_ne_bytes());
    request.extend_from_slice(&[192, 0, 2, 7]);
    assert_eq!(
        codec::neighbor_get(&request),
        Ok((2, "192.0.2.7".parse().unwrap(), false))
    );
    request.truncate(12);
    for index in [3u32, 4] {
        request.extend_from_slice(&8u16.to_ne_bytes());
        request.extend_from_slice(&8u16.to_ne_bytes());
        request.extend_from_slice(&index.to_ne_bytes());
    }
    assert_eq!(codec::neighbor_dump(&request), Ok((4, false)));
    request[10] = 8;
    assert_eq!(codec::neighbor_dump(&request), Ok((4, true)));
    assert_eq!(codec::neighbor_dump(&[10]), Ok((0, false)));
}
