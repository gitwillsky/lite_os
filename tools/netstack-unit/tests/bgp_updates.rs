//! RFC 7606 的组合错误会保留陈旧路由或误重置邻居；格式夹具无法覆盖实际 Session 的优先级与增量决定。
use ::bgp::{Aggregator, Change, Effect, Handling, Input, Notice, Session, State};
use core::net::Ipv4Addr;
use packet_wire::bgp::{
    Frame,
    attributes::{AsPath, AsnWidth, Attribute, Attributes, SegmentKind, Value},
    nlri::Family,
};

mod bgp_peer;
use bgp_peer::{attributes, base, established, receive, update};

fn closed<'a>(session: &mut Session, bytes: &'a [u8]) -> Notice<'a> {
    let effect = session
        .on(4, Input::Frame(Frame::parse(bytes, false).unwrap()))
        .into_iter()
        .next()
        .unwrap();
    let Effect::Close {
        received: None,
        notice: Some(notice),
        withdraw_routes: true,
        send_extended: false,
    } = effect
    else {
        panic!("expected close, got {effect:?}")
    };
    assert_eq!(session.state(), State::Idle);
    notice
}

#[test]
fn accepted_update_preserves_first_attributes_and_orders_withdraw_before_announce() {
    let mut list = base(false, true);
    list.extend_from_slice(&[
        Attribute {
            flags: 0x40,
            code: 1,
            data: &[99],
        },
        Attribute {
            flags: 0xc0,
            code: 250,
            data: &[1, 2],
        },
        Attribute {
            flags: 0x80,
            code: 251,
            data: &[1],
        },
    ]);
    let bytes = update(&[24, 203, 0, 113], &attributes(&list), &[24, 203, 0, 113]);
    let mut session = established(false, true);
    let received = receive(&mut session, &bytes);
    assert_eq!(received.handling(), Handling::Accept);
    assert_eq!(received.message_bytes(), bytes);
    assert_eq!(
        received
            .received_attributes()
            .map(|attribute| attribute.code)
            .collect::<Vec<_>>(),
        [1, 3, 250]
    );
    let changes = received.changes().collect::<Vec<_>>();
    let Change::Withdraw(withdrawn, None) = changes[0] else {
        panic!("withdraw first")
    };
    let Change::Announce { prefix, path_id: None, next_hop } = changes[1] else {
        panic!("announce second")
    };
    assert_eq!(withdrawn, prefix);
    assert_eq!(
        prefix.address,
        "203.0.113.0".parse::<core::net::IpAddr>().unwrap()
    );
    assert_eq!(
        next_hop.address,
        "192.0.2.2".parse::<core::net::IpAddr>().unwrap()
    );
    assert_eq!(received.attribute_errors().count(), 0);
}

#[test]
fn malformed_routing_attributes_withdraw_all_announcements_instead_of_ignoring_them() {
    for (internal, malformed) in [
        (
            false,
            Attribute {
                flags: 0x40,
                code: 1,
                data: &[3],
            },
        ),
        (
            false,
            Attribute {
                flags: 0x40,
                code: 2,
                data: &[2, 0],
            },
        ),
        (
            false,
            Attribute {
                flags: 0x40,
                code: 2,
                data: &[2, 1, 0, 0, 0, 0],
            },
        ),
        (
            false,
            Attribute {
                flags: 0x40,
                code: 3,
                data: &[0; 4],
            },
        ),
        (
            false,
            Attribute {
                flags: 0x80,
                code: 4,
                data: &[0; 3],
            },
        ),
        (
            true,
            Attribute {
                flags: 0x40,
                code: 5,
                data: &[0; 3],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 8,
                data: &[0; 3],
            },
        ),
        (
            true,
            Attribute {
                flags: 0x80,
                code: 9,
                data: &[0; 3],
            },
        ),
        (
            true,
            Attribute {
                flags: 0x80,
                code: 10,
                data: &[],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 16,
                data: &[0; 7],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 32,
                data: &[0; 11],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 1,
                data: &[0],
            },
        ),
    ] {
        let mut list = base(internal, true);
        if let Some(index) = list
            .iter()
            .position(|attribute| attribute.code == malformed.code)
        {
            list[index] = malformed;
        } else {
            list.push(malformed);
        }
        let bytes = update(&[24, 203, 0, 114], &attributes(&list), &[24, 203, 0, 113]);
        let mut session = established(internal, true);
        let received = receive(&mut session, &bytes);
        assert_eq!(
            received.handling(),
            Handling::TreatAsWithdraw,
            "code {}",
            malformed.code
        );
        assert!(
            received
                .attribute_errors()
                .any(|code| code == malformed.code)
        );
        assert_eq!(received.received_attributes().count(), 0);
        assert_eq!(received.changes().count(), 2);
        assert!(
            received
                .changes()
                .all(|change| matches!(change, Change::Withdraw(_, None)))
        );
    }
    for missing in [1, 2, 3, 5] {
        let list = base(true, true)
            .into_iter()
            .filter(|attribute| attribute.code != missing)
            .collect::<Vec<_>>();
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let mut session = established(true, true);
        let received = receive(&mut session, &bytes);
        assert_eq!(received.handling(), Handling::TreatAsWithdraw);
        assert!(received.attribute_errors().any(|code| code == missing));
    }
}

#[test]
fn discard_only_attributes_do_not_withdraw_valid_routes_or_accept_later_duplicates() {
    for (four, malformed) in [
        (
            true,
            Attribute {
                flags: 0x40,
                code: 6,
                data: &[1],
            },
        ),
        (
            true,
            Attribute {
                flags: 0xc0,
                code: 7,
                data: &[0; 7],
            },
        ),
        (
            true,
            Attribute {
                flags: 0xc0,
                code: 7,
                data: &[0, 0, 0, 0, 192, 0, 2, 1],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 17,
                data: &[],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 18,
                data: &[0; 7],
            },
        ),
        (
            false,
            Attribute {
                flags: 0xc0,
                code: 18,
                data: &[0, 0, 0, 0, 192, 0, 2, 1],
            },
        ),
    ] {
        let mut list = base(false, four);
        list.push(malformed);
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let mut session = established(false, four);
        let received = receive(&mut session, &bytes);
        assert_eq!(received.handling(), Handling::Accept);
        assert!(
            received
                .attribute_errors()
                .any(|code| code == malformed.code)
        );
        assert!(
            !received
                .received_attributes()
                .any(|attribute| attribute.code == malformed.code)
        );
        assert!(matches!(
            received.changes().next(),
            Some(Change::Announce { .. })
        ));
    }
    let mut list = base(false, true);
    list.extend_from_slice(&[
        Attribute {
            flags: 0x40,
            code: 5,
            data: &[1],
        },
        Attribute {
            flags: 0x80,
            code: 9,
            data: &[1],
        },
        Attribute {
            flags: 0x80,
            code: 10,
            data: &[],
        },
        Attribute {
            flags: 0xc0,
            code: 17,
            data: &[],
        },
        Attribute {
            flags: 0xc0,
            code: 18,
            data: &[],
        },
    ]);
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    let mut session = established(false, true);
    let received = receive(&mut session, &bytes);
    assert_eq!(received.handling(), Handling::Accept);
    assert_eq!(
        received
            .received_attributes()
            .map(|attribute| attribute.code)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert!(received.path().unwrap().contains(65001));
    assert_eq!(received.aggregator(), None);
    // 普通重复只保留首项，不能让好副本修复先到的坏 ORIGIN。
    let mut list = base(false, true);
    list[0].data = &[3];
    list.push(Attribute {
        flags: 0x40,
        code: 1,
        data: &[0],
    });
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    assert_eq!(
        receive(&mut established(false, true), &bytes).handling(),
        Handling::TreatAsWithdraw
    );
}

#[test]
fn mp_reachability_preserves_both_next_hops_and_eor_without_forging_missing_attributes() {
    let mut reach = vec![0, 2, 1, 32];
    reach.extend_from_slice(
        &"2001:db8::1"
            .parse::<core::net::Ipv6Addr>()
            .unwrap()
            .octets(),
    );
    reach.extend_from_slice(&"fe80::1".parse::<core::net::Ipv6Addr>().unwrap().octets());
    reach.extend_from_slice(&[0, 32, 0x20, 1, 0x0d, 0xb8]);
    let mut list = base(false, true);
    list.retain(|attribute| attribute.code != 3);
    list.insert(
        0,
        Attribute {
            flags: 0x80,
            code: 14,
            data: &reach,
        },
    );
    let bytes = update(&[], &attributes(&list), &[]);
    let received = receive(&mut established(false, true), &bytes);
    assert_eq!(received.handling(), Handling::Accept);
    let Change::Announce { prefix, path_id: None, next_hop } = received.changes().next().unwrap() else {
        panic!("IPv6 announce")
    };
    assert_eq!(
        prefix.address,
        "2001:db8::".parse::<core::net::IpAddr>().unwrap()
    );
    assert_eq!(
        next_hop.address,
        "2001:db8::1".parse::<core::net::IpAddr>().unwrap()
    );
    assert_eq!(
        next_hop.link_local,
        Some("fe80::1".parse::<core::net::Ipv6Addr>().unwrap())
    );
    let mut invalid = reach.clone();
    invalid[4..20].copy_from_slice(&"fe80::1".parse::<core::net::Ipv6Addr>().unwrap().octets());
    let mut list = base(false, true);
    list.retain(|attribute| attribute.code != 3);
    list.push(Attribute {
        flags: 0x80,
        code: 14,
        data: &invalid,
    });
    let rejected = update(&[], &attributes(&list), &[]);
    assert_eq!(
        receive(&mut established(false, true), &rejected).handling(),
        Handling::TreatAsWithdraw
    );
    let bytes = update(
        &[],
        &attributes(&[Attribute {
            flags: 0x80,
            code: 15,
            data: &[0, 2, 1],
        }]),
        &[],
    );
    let received = receive(&mut established(false, true), &bytes);
    assert_eq!(received.end_of_rib(), Some(Family::Ipv6));
    assert_eq!(received.changes().count(), 0);
    let bytes = update(&[], &[], &[]);
    assert_eq!(
        receive(&mut established(false, true), &bytes).end_of_rib(),
        Some(Family::Ipv4)
    );
    let bytes = update(&[24, 203, 0, 113], &[], &[]);
    let received = receive(&mut established(false, true), &bytes);
    assert_eq!(received.end_of_rib(), None);
    assert!(matches!(
        received.changes().next(),
        Some(Change::Withdraw(_, None))
    ));
}

#[test]
fn truncated_attributes_use_locatable_nlri_and_unlocatable_routes_reset_the_session() {
    let mut list = attributes(&base(false, true));
    list.extend_from_slice(&[0x40, 1]);
    let bytes = update(&[], &list, &[24, 203, 0, 113]);
    let received = receive(&mut established(false, true), &bytes);
    assert!(received.framing_error());
    assert_eq!(received.handling(), Handling::TreatAsWithdraw);
    assert!(matches!(
        received.changes().next(),
        Some(Change::Withdraw(_, None))
    ));
    let bytes = update(&[], &list, &[]);
    let notice = closed(&mut established(false, true), &bytes);
    assert_eq!((notice.code, notice.subcode), (3, 1));
    let reach = [
        0, 2, 1, 16, 0x20, 1, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 32, 0x20, 1, 0x0d,
        0xb8,
    ];
    let mut list = vec![Attribute {
        flags: 0x80,
        code: 14,
        data: &reach,
    }];
    list.extend(base(false, true));
    let mut list = attributes(&list);
    list.extend_from_slice(&[0x40, 1]);
    let bytes = update(&[], &list, &[]);
    let received = receive(&mut established(false, true), &bytes);
    assert!(received.framing_error());
    assert!(
        matches!(received.changes().next(), Some(Change::Withdraw(prefix, None)) if prefix.address.is_ipv6())
    );
    let bytes = update(&[], &attributes(&base(false, true)), &[]);
    let received = receive(&mut established(false, true), &bytes);
    assert_eq!(received.handling(), Handling::Accept);
    assert_eq!(received.end_of_rib(), None);
}

#[test]
fn duplicate_or_malformed_mp_and_unknown_well_known_attributes_win_over_weaker_errors() {
    let unknown_mp = Attribute {
        flags: 0x80,
        code: 14,
        data: &[0x12, 0x34, 1, 0, 0, 255],
    };
    let mut list = base(false, true);
    list.extend_from_slice(&[unknown_mp, unknown_mp]);
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    assert_eq!(closed(&mut established(false, true), &bytes).subcode, 1);
    let mut list = base(false, true);
    list.push(unknown_mp);
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    assert_eq!(
        receive(&mut established(false, true), &bytes)
            .changes()
            .count(),
        1
    );
    for (flags, data) in [
        (0x80, &[0, 2, 1, 17, 0][..]),
        (0xc0, &[0, 2, 1, 0, 0][..]),
        (0x80, &[0, 2, 1][..]),
    ] {
        let mut list = base(false, true);
        list.push(Attribute {
            flags,
            code: 14,
            data,
        });
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let notice = closed(&mut established(false, true), &bytes);
        assert_eq!((notice.code, notice.subcode), (3, 9));
        assert_eq!(notice.data.last(), data.last());
    }
    let mut list = base(false, true);
    list[0].data = &[3];
    list.push(Attribute {
        flags: 0x40,
        code: 99,
        data: &[7],
    });
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    let notice = closed(&mut established(false, true), &bytes);
    assert_eq!(
        notice,
        Notice {
            code: 3,
            subcode: 2,
            data: &[0x40, 99, 1, 7]
        }
    );
    let bytes = update(&[], &attributes(&base(false, true)), &[33]);
    assert_eq!(closed(&mut established(false, true), &bytes).subcode, 10);
}

#[test]
fn old_peer_as4_auxiliary_fields_discard_bad_values_and_remove_confederation_segments() {
    let path = [3, 1, 0, 0, 0xfd, 0xe8, 2, 1, 0, 1, 0, 1];
    let mut list = base(false, false);
    list.extend_from_slice(&[
        Attribute {
            flags: 0xc0,
            code: 17,
            data: &path,
        },
        Attribute {
            flags: 0xc0,
            code: 18,
            data: &[0, 1, 0, 1, 192, 0, 2, 9],
        },
    ]);
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    let received = receive(&mut established(false, false), &bytes);
    assert_eq!(received.handling(), Handling::Accept);
    let segment = received.path().unwrap().segments().next().unwrap();
    assert_eq!(segment.kind(), SegmentKind::Sequence);
    assert!(segment.asns().eq([65537]));
    assert_eq!(
        received.aggregator(),
        None // 没有基础 AGGREGATOR，孤立的辅助值不能自行成为聚合器。
    );
    assert!(received.attribute_errors().any(|code| code == 17));
    assert!(
        !received
            .received_attributes()
            .any(|attribute| matches!(attribute.code, 2 | 7 | 17 | 18))
    );
}

#[test]
fn reconstructed_as4_path_preserves_selection_length_sets_and_only_adjacent_confederations() {
    use SegmentKind::{
        ConfederationSequence as Confed, ConfederationSet as ConfedSet, Sequence as Seq, Set,
    };
    // 错误拼接会改变最佳路径长度、拆散集合或隐藏真实四字节本地 AS；格式/错误分类用例不覆盖这些边界。
    for (base_segments, auxiliary, expected, length) in [
        (
            vec![(Seq, vec![65010, 65011, 23456, 23456])],
            vec![(Seq, vec![70000, 80000])],
            vec![(Seq, vec![65010, 65011]), (Seq, vec![70000, 80000])],
            4,
        ),
        (
            vec![(Set, vec![65010, 65011]), (Seq, vec![23456, 23456])],
            vec![(Seq, vec![70000, 80000])],
            vec![(Set, vec![65010, 65011]), (Seq, vec![70000, 80000])],
            3,
        ),
        (
            vec![(Seq, vec![65010, 23456])],
            vec![(Set, vec![70000, 80000])],
            vec![(Seq, vec![65010]), (Set, vec![70000, 80000])],
            2,
        ),
        (
            vec![(Seq, vec![65010])],
            vec![(Seq, vec![70000, 80000])],
            vec![(Seq, vec![65010])],
            1,
        ),
        (
            vec![(Confed, vec![65020]), (Seq, vec![23456, 23456])],
            vec![(Seq, vec![70000, 80000])],
            vec![(Confed, vec![65020]), (Seq, vec![70000, 80000])],
            2,
        ),
        (
            vec![
                (Seq, vec![65010]),
                (ConfedSet, vec![65020, 65021]),
                (Seq, vec![23456]),
            ],
            vec![(Seq, vec![70000])],
            vec![
                (Seq, vec![65010]),
                (ConfedSet, vec![65020, 65021]),
                (Seq, vec![70000]),
            ],
            2,
        ),
        (
            vec![
                (Seq, vec![65010, 23456, 23456]),
                (Confed, vec![65020]),
                (Seq, vec![23456]),
            ],
            vec![(Seq, vec![70000, 80000, 90000])],
            vec![(Seq, vec![65010]), (Seq, vec![70000, 80000, 90000])],
            4,
        ),
        (
            vec![(Seq, vec![65010, 23456])],
            vec![(Confed, vec![65020])],
            vec![(Seq, vec![65010, 23456])],
            2,
        ),
        (vec![], vec![(Seq, vec![70000])], vec![], 0),
        (
            vec![(Confed, vec![65020])],
            vec![(Seq, vec![70000])],
            vec![(Confed, vec![65020])],
            0,
        ),
    ] {
        let encode = |segments: &[(SegmentKind, Vec<u32>)], width| {
            let mut bytes = vec![0; 256];
            let length = AsPath::emit(
                &mut bytes,
                width,
                segments
                    .iter()
                    .map(|(kind, asns)| (*kind, asns.iter().copied())),
            )
            .unwrap();
            bytes.truncate(length);
            bytes
        };
        let base_path = encode(&base_segments, AsnWidth::Two);
        let as4_path = encode(&auxiliary, AsnWidth::Four);
        let mut list = base(true, false);
        list[1].data = &base_path;
        list.push(Attribute {
            flags: 0xc0,
            code: 17,
            data: &as4_path,
        });
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let config = ::bgp::Config {
            confederation_peer: Some(::bgp::PeerKind::Internal),
            ..bgp_peer::config(true)
        };
        let received = receive(&mut bgp_peer::established_with(config, false), &bytes);
        let path = received.path().unwrap();
        let actual = path
            .segments()
            .map(|segment| (segment.kind(), segment.asns().collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(path.hop_count(), length);
        for asn in [65010, 65020, 65021, 23456, 70000, 80000, 90000] {
            assert_eq!(
                path.contains(asn),
                expected.iter().any(|(_, asns)| asns.contains(&asn))
            );
        }
    }
}

#[test]
fn old_aggregator_overrides_both_as4_fields_and_trans_aggregator_uses_auxiliary_identity() {
    for (base_asn, auxiliary, expected_asn, expected_path) in [
        (Some(65010u16), true, Some(65010u32), 65001),
        (Some(23456), true, Some(70000), 70000),
        (Some(23456), false, Some(23456), 70000),
        (None, true, None, 70000),
    ] {
        let mut list = base(false, false);
        let mut aggregator = [0; 6];
        if let Some(asn) = base_asn {
            aggregator[..2].copy_from_slice(&asn.to_be_bytes());
            aggregator[2..].copy_from_slice(&[192, 0, 2, 7]);
            list.push(Attribute {
                flags: 0xc0,
                code: 7,
                data: &aggregator,
            });
        }
        list.push(Attribute {
            flags: 0xc0,
            code: 17,
            data: &[2, 1, 0, 1, 0x11, 0x70],
        });
        if auxiliary {
            list.push(Attribute {
                flags: 0xc0,
                code: 18,
                data: &[0, 1, 0x11, 0x70, 192, 0, 2, 8],
            });
        }
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let received = receive(&mut established(false, false), &bytes);
        assert_eq!(received.aggregator().map(|value| value.asn), expected_asn);
        if let Some(Aggregator {
            asn, identifier, ..
        }) = received.aggregator()
        {
            assert_eq!(
                identifier,
                Ipv4Addr::new(192, 0, 2, if asn == 70000 { 8 } else { 7 })
            );
        }
        assert!(
            received
                .path()
                .unwrap()
                .segments()
                .flat_map(|segment| segment.asns())
                .eq([expected_path])
        );
    }
}

#[test]
fn exported_paths_use_negotiated_width_and_only_required_as4_attributes_without_copying_confederations()
 {
    // 接收用例不能捕获发送侧丢失真实 ASN、泄露联盟段、给 NEW 邻居发送辅助字段或错误聚合器宽度。
    for (four_path, width, aggregator, codes) in [
        (true, AsnWidth::Two, Some(70000), &[2, 17, 7, 18][..]),
        (true, AsnWidth::Two, Some(65010), &[2, 17, 7]),
        (true, AsnWidth::Two, None, &[2, 17]),
        (false, AsnWidth::Two, Some(70000), &[2, 7, 18]),
        (false, AsnWidth::Two, Some(65010), &[2, 7]),
        (false, AsnWidth::Two, None, &[2]),
        (true, AsnWidth::Four, Some(70000), &[2, 7]),
        (true, AsnWidth::Four, None, &[2]),
    ] {
        let sequence = [65001, if four_path { 70000 } else { 65535 }];
        let mut encoded = [0; 64];
        let length = AsPath::emit(
            &mut encoded,
            AsnWidth::Four,
            [
                (SegmentKind::ConfederationSequence, &[65020][..]),
                (SegmentKind::Sequence, &sequence),
                (SegmentKind::Set, &[65002, 65003]),
            ]
            .into_iter()
            .map(|(kind, values)| (kind, values.iter().copied())),
        )
        .unwrap();
        let mut list = base(true, true);
        list[1].data = &encoded[..length];
        let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
        let received = receive(
            &mut bgp_peer::established_with(
                ::bgp::Config {
                    confederation_peer: Some(::bgp::PeerKind::Internal),
                    ..bgp_peer::config(true)
                },
                true,
            ),
            &bytes,
        );
        let path = received.path().unwrap();
        let aggregator = aggregator.map(|asn| Aggregator {
            asn,
            identifier: Ipv4Addr::new(192, 0, 2, 7),
            partial: false,
        });
        let mut output = [0xa5; 256];
        let length = path
            .emit_attributes(&mut output, width, aggregator)
            .unwrap();
        assert_eq!(output[length], 0xa5);
        let emitted = Attributes::parse(&output[..length])
            .unwrap()
            .collect::<Vec<_>>();
        assert!(
            emitted
                .iter()
                .map(|attribute| attribute.code)
                .eq(codes.iter().copied())
        );
        let Value::AsPath(base_path) = emitted[0].decode(width).unwrap() else {
            panic!("base path")
        };
        assert_eq!(base_path.segments().count(), 3);
        assert!(base_path.segments().nth(1).unwrap().asns().eq([
            65001,
            if width == AsnWidth::Two && four_path {
                23456
            } else {
                sequence[1]
            }
        ]));
        if let Some(auxiliary) = emitted.iter().find(|attribute| attribute.code == 17) {
            let Value::AsPath(as4) = auxiliary.decode(width).unwrap() else {
                panic!("AS4 path")
            };
            assert!(
                as4.segments()
                    .map(|segment| segment.kind)
                    .eq([SegmentKind::Sequence, SegmentKind::Set])
            );
            assert!(as4.segments().next().unwrap().asns().eq(sequence));
        }
        // 实际 OLD/NEW Session 再次接收生成的属性，必须恢复路径与聚合器，而不是只检查自己写出的长度。
        let mut roundtrip = base(true, width == AsnWidth::Four);
        roundtrip.retain(|attribute| attribute.code != 2);
        roundtrip.extend(emitted);
        let bytes = update(&[], &attributes(&roundtrip), &[24, 203, 0, 113]);
        let received = receive(
            &mut bgp_peer::established_with(
                ::bgp::Config {
                    confederation_peer: Some(::bgp::PeerKind::Internal),
                    ..bgp_peer::config(true)
                },
                width == AsnWidth::Four,
            ),
            &bytes,
        );
        assert!(
            received
                .path()
                .unwrap()
                .segments()
                .flat_map(|segment| segment.asns())
                .eq([65020, sequence[0], sequence[1], 65002, 65003])
        );
        assert_eq!(received.aggregator(), aggregator);
        for capacity in 0..length {
            let mut short = [0xa5; 256];
            assert!(
                path.emit_attributes(&mut short[..capacity], width, aggregator)
                    .is_err()
            );
            assert_eq!(short, [0xa5; 256]);
        }
        let previous = output;
        assert_eq!(
            path.emit_attributes(
                &mut output,
                width,
                Some(Aggregator {
                    asn: 0,
                    identifier: Ipv4Addr::LOCALHOST,
                    partial: false
                })
            ),
            Err(packet_wire::PacketError::Malformed)
        );
        assert_eq!(output, previous);
    }
}

#[test]
fn old_export_cannot_map_non_mappable_confederation_members_into_auxiliary_paths() {
    let mut list = base(true, true);
    list[1].data = &[3, 1, 0, 1, 0x11, 0x70, 2, 1, 0, 0, 0xfd, 0xe9];
    let bytes = update(&[], &attributes(&list), &[24, 203, 0, 113]);
    let received = receive(
        &mut bgp_peer::established_with(
            ::bgp::Config {
                confederation_peer: Some(::bgp::PeerKind::Internal),
                ..bgp_peer::config(true)
            },
            true,
        ),
        &bytes,
    );
    let mut output = [0xa5; 64];
    assert_eq!(
        received
            .path()
            .unwrap()
            .emit_attributes(&mut output, AsnWidth::Two, None),
        Err(packet_wire::PacketError::Malformed)
    );
    assert_eq!(output, [0xa5; 64]);
    // NEW 方向无需 AS4 辅助属性，真实联盟号仍在基础四字节 AS_PATH 中。
    let length = received
        .path()
        .unwrap()
        .emit_attributes(&mut output, AsnWidth::Four, None)
        .unwrap();
    let attribute = Attributes::parse(&output[..length])
        .unwrap()
        .next()
        .unwrap();
    let Value::AsPath(path) = attribute.decode(AsnWidth::Four).unwrap() else {
        panic!("AS_PATH")
    };
    assert!(path.segments().next().unwrap().asns().eq([70000]));
}
