//! 直接编码 Linux 生产快照投影，验证可变 rtnexthop framing，不构造替代 codec。
extern crate alloc;
mod net {
    pub(crate) use network_protocol::{InterfaceSnapshot, Ipv4RouteKind, RoutingSnapshot};
}
#[path = "../../../system/linuxd/src/net/rtnetlink/codec.rs"]
#[expect(
    dead_code,
    reason = "本夹具只覆盖路由消息；link/address 与请求分派由来宾 ABI 用例覆盖"
)]
mod codec;
use core::net::Ipv4Addr;
use network_protocol::routes::{NextHopConfiguration, NextHopMember, RouteAttributes};
use network_protocol::{Ipv4RouteKind, Ipv4RouteMetrics, RoutingSnapshot};

fn snapshot(members: &[NextHopMember], count: usize) -> RoutingSnapshot {
    let route = RouteAttributes {
        destination: Ipv4Addr::UNSPECIFIED,
        prefix_length: 0,
        table: 254,
        metric: 3,
        scope: 0,
        protocol: 3,
        kind: Ipv4RouteKind::Unicast,
        preferred_source: None,
        metrics: Ipv4RouteMetrics::default(),
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
    let mut members = [
        NextHopMember {
            interface_index: 2,
            gateway: Some(Ipv4Addr::new(192, 0, 2, 1)),
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
    for (kind, value) in [
        (Ipv4RouteKind::Blackhole, 6),
        (Ipv4RouteKind::Prohibit, 8),
        (Ipv4RouteKind::Unreachable, 7),
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
            metrics: Ipv4RouteMetrics::default(),
        };
        let bytes = RoutingSnapshot::encode(
            [].into_iter(),
            [].into_iter(),
            [(attributes, None, false)].into_iter(),
        )
        .unwrap();
        let dump = codec::RouteDump::new(RoutingSnapshot::parse(bytes).unwrap()).unwrap();
        out = Vec::with_capacity(dump.maximum_message_size());
        assert!(dump.append(0, 0, 0, 1, &mut out));
        assert_eq!(out[codec::HEADER_LENGTH + 7], value);
        assert!(codec::find_attribute(&out[codec::HEADER_LENGTH..], 12, 4).is_none());
        assert!(codec::find_attribute(&out[codec::HEADER_LENGTH..], 12, 5).is_none());
    }
}
