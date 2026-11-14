use super::{bgp_peer, routing};
use bgp::{
    AdjRibOut, Change, Config, ExportContext, Peer, PeerKind, Reachability, RoutingDomain,
    SelectionContext, SourcePeer,
};
use core::num::NonZeroU32;
use packet_wire::bgp::{
    Frame, Message,
    attributes::{Attribute, MpReach, MpUnreach},
    nlri::{Family, Nlri, Prefix},
    open::CapabilityValue,
};

fn upstream_update(family: Family, prefix: Prefix, ids: &[u32], withdraw: bool) -> Vec<u8> {
    let mut encoded = [0; 64];
    let len = Nlri::emit_paths(
        &mut encoded,
        family,
        ids.iter().map(|&id| (prefix, Some(id))),
    )
    .unwrap();
    let nlri = &encoded[..len];
    let mut attributes = bgp_peer::base(false, true);
    if family == Family::Ipv4 {
        if withdraw {
            return bgp_peer::update(nlri, &[], &[]);
        }
        return bgp_peer::update(&[], &bgp_peer::attributes(&attributes), nlri);
    }
    let mut mp = [0; 128];
    let nlri = Nlri::parse_with_path_ids(nlri, family, true).unwrap();
    let (code, len) = if withdraw {
        attributes.clear();
        (15, MpUnreach::emit(&mut mp, 1, nlri).unwrap())
    } else {
        attributes.truncate(2);
        let hop = "2001:db8::2"
            .parse::<core::net::Ipv6Addr>()
            .unwrap()
            .octets();
        (14, MpReach::emit(&mut mp, 1, &hop, nlri).unwrap())
    };
    attributes.push(Attribute {
        code,
        flags: 0x80,
        data: &mp[..len],
    });
    bgp_peer::update(&[], &bgp_peer::attributes(&attributes), &[])
}

#[test]
fn add_path_exports_each_confirmed_path_and_withdraws_only_its_local_id() {
    let receive_only = Config {
        receive_add_path: true,
        ..bgp_peer::config(false)
    };
    let send_only = Config {
        send_add_path: true,
        ..bgp_peer::config(false)
    };
    let both = Config {
        receive_add_path: true,
        ..send_only
    };
    let mut bytes = [0; 128];
    let length = both
        .emit_open(bgp::OpenOffer::Capabilities, &mut bytes)
        .unwrap();
    let Message::Open(open) = Frame::parse(&bytes[..length], false)
        .unwrap()
        .message()
        .unwrap()
    else {
        panic!("expected OPEN");
    };
    let mut offers = open
        .parameters()
        .flat_map(|parameter| parameter.capabilities().unwrap())
        .filter_map(|capability| match capability.decode().unwrap() {
            CapabilityValue::AddPath(offer) => Some(offer),
            _ => None,
        });
    let offer = offers.next().unwrap();
    assert!(offers.next().is_none());
    assert!(
        [1, 2]
            .into_iter()
            .all(|afi| offer.sends(afi, 1) && offer.receives(afi, 1))
    );
    let advertised =
        bgp_peer::established_with_capabilities(send_only, true, &[69, 8, 0, 1, 1, 1, 0, 2, 1, 1]);
    let flags = advertised.negotiated().unwrap();
    assert_eq!(flags.send_add_path, [true; 2]);
    assert_eq!(flags.receive_add_path, [false; 2]);

    for family in [Family::Ipv4, Family::Ipv6] {
        let prefix = match family {
            Family::Ipv4 => routing::prefix(),
            Family::Ipv6 => Prefix {
                address: "2001:db8:1::".parse().unwrap(),
                length: 64,
            },
        };
        let mut upstream = bgp_peer::established_with_capabilities(
            receive_only,
            true,
            &[69, 8, 0, 1, 1, 2, 0, 2, 1, 2],
        );
        let mut remote = bgp_peer::established_with_capabilities(
            Config {
                local_as: 65002,
                peer_as: 65000,
                ..receive_only
            },
            true,
            &[69, 8, 0, 1, 1, 2, 0, 2, 1, 2],
        );
        let selection = SelectionContext::new(65000, send_only.identifier, None, None).unwrap();
        let mut domain = RoutingDomain::try_new(selection, [true; 2]).unwrap();
        domain
            .add_peer(
                Peer {
                    id: 1,
                    asn: 65001,
                    address: "192.0.2.2".parse().unwrap(),
                    kind: PeerKind::External,
                    identifier: "192.0.2.2".parse().unwrap(),
                    route_reflector_client: false,
                },
                SourcePeer {
                    preference: NonZeroU32::new(20).unwrap(),
                    next_hop_interface: None,
                },
            )
            .unwrap();
        let frame = upstream_update(family, prefix, &[11, 12, 13], false);
        domain
            .receive(1, &bgp_peer::receive(&mut upstream, &frame))
            .unwrap();
        fn classify<'a>(
            _: Prefix,
            _: &Peer,
            path_id: Option<u32>,
            route: &'a bgp::ReceivedRoute,
        ) -> Result<
            (
                bgp::NextHop,
                Option<u32>,
                Reachability,
                &'a bgp::RouteAttributes,
            ),
            bgp::OutOfMemory,
        > {
            // 编号 13 模拟下一跳未通过安装后的可达性检查。
            Ok((
                route.next_hop(),
                Some(100),
                if path_id == Some(13) {
                    Reachability::Unresolved
                } else {
                    Reachability::Resolved {
                        interior_cost: Some(0),
                    }
                },
                route.attributes(),
            ))
        }
        let update = domain.prepare(family, classify).unwrap();
        assert!(update.forwarding_changed());
        assert_eq!(update.commit(), [prefix]);
        let context = ExportContext {
            local_as: 65000,
            local_address: match family {
                Family::Ipv4 => "192.0.2.1".parse().unwrap(),
                Family::Ipv6 => "2001:db8::1".parse().unwrap(),
            },
            ipv6_unicast: true,
            ..routing::context()
        };
        let policy = routing::policy();
        let mut output = AdjRibOut::new(flags.send_add_path);
        output.reconsider(family);
        let initial = output
            .prepare(&mut domain, Some(&policy), context, 0)
            .unwrap()
            .unwrap();
        assert!(output.is_empty());
        let received = bgp_peer::receive(&mut remote, &initial);
        assert!(received.changes().eq([1, 2].map(|id| Change::Announce {
            prefix,
            path_id: Some(id),
            next_hop: bgp::NextHop {
                address: context.local_address,
                link_local: None
            },
        })));
        output.sent(1);

        // 删除备选不会改转发表，仍须在既有 MRAI 截止后撤回正确的本端编号。
        let frame = upstream_update(family, prefix, &[12], true);
        domain
            .receive(1, &bgp_peer::receive(&mut upstream, &frame))
            .unwrap();
        let update = domain.prepare(family, classify).unwrap();
        assert!(!update.forwarding_changed());
        assert_eq!(update.commit(), [prefix]);
        output.changed(&domain, prefix, 2).unwrap();
        assert!(
            output
                .prepare(&mut domain, Some(&policy), context, 2)
                .unwrap()
                .is_none()
        );
        let withdrawal = output
            .prepare(&mut domain, Some(&policy), context, 31)
            .unwrap()
            .unwrap();
        assert!(
            bgp_peer::receive(&mut remote, &withdrawal)
                .changes()
                .eq([Change::Withdraw(prefix, Some(2))])
        );
        assert_eq!(output.len(), 2);
        output.sent(31);
        assert!(output.advertised_paths(prefix).eq([Some(1)]));

        // 路由刷新使用存活路径原编号，且保持同一发送间隔。
        output.refresh(family, false);
        assert!(
            output
                .prepare(&mut domain, Some(&policy), context, 32)
                .unwrap()
                .is_none()
        );
        let replay = output
            .prepare(&mut domain, Some(&policy), context, 61)
            .unwrap()
            .unwrap();
        assert!(matches!(
            bgp_peer::receive(&mut remote, &replay).changes().next(),
            Some(Change::Announce {
                path_id: Some(1),
                ..
            })
        ));
        output.sent(61);
        assert!(output.advertised_paths(prefix).eq([Some(1)]));
    }
}
