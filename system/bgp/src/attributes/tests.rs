//! 保护完整属性身份和嵌套驻留的真实生命周期；既有 wire/Session/路径用例没有完整属性 owner。
use super::*;
use crate::{Config, Handling, Negotiated};
use alloc::vec;
use packet_wire::bgp::{
    Frame, Message,
    attributes::Attributes,
    nlri::{Family, Nlri},
    update::Update,
};

fn receive<'a>(
    bytes: &'a mut [u8],
    extra: &[Attribute<'_>],
    four: bool,
    next_hop: [u8; 4],
    announced: &[u8],
) -> ReceivedUpdate<'a> {
    let mut attributes = vec![
        Attribute {
            flags: 0x40,
            code: 1,
            data: &[0],
        },
        Attribute {
            flags: 0x40,
            code: 2,
            data: if four {
                &[2, 1, 0, 0, 0xfd, 0xe9]
            } else {
                &[2, 1, 0xfd, 0xe9]
            },
        },
        Attribute {
            flags: 0x40,
            code: 3,
            data: &next_hop,
        },
        Attribute {
            flags: 0x40,
            code: 5,
            data: &[0, 0, 0, 100],
        },
    ];
    attributes.extend_from_slice(extra);
    let mut encoded = vec![0; 4096];
    let size = Attributes::emit(&mut encoded, &attributes).unwrap();
    let size = Update::emit(
        bytes,
        Nlri::parse(&[], Family::Ipv4).unwrap(),
        Attributes::parse(&encoded[..size]).unwrap(),
        Nlri::parse(announced, Family::Ipv4).unwrap(),
        false,
    )
    .unwrap();
    let raw = &bytes[..size];
    let Message::Update(update) = Frame::parse(raw, false).unwrap().message().unwrap() else {
        panic!("UPDATE")
    };
    let config = Config {
        local_as: 65001,
        peer_as: 65001,
        identifier: Ipv4Addr::new(192, 0, 2, 1),
        hold_seconds: 90,
        retry_seconds: 5,
        passive: false,
        ipv4_unicast: true,
        ipv6_unicast: true,
        route_refresh: true,
        extended_messages: false,
    };
    let peer = Negotiated {
        peer_as: 65001,
        peer_identifier: Ipv4Addr::new(192, 0, 2, 2),
        hold_seconds: 90,
        ipv4_unicast: true,
        ipv6_unicast: true,
        four_octet_asn: four,
        send_extended: false,
        receive_extended: false,
        send_refresh: true,
        receive_refresh: true,
    };
    crate::received::capture(config, peer, update, raw).unwrap()
}

#[test]
fn reordered_attributes_and_community_sets_share_content_but_next_hop_and_nlri_do_not_enter_identity()
 {
    let mut pool = AttributePool::try_new().unwrap();
    let mut bytes = vec![0; 4096];
    let first = [
        Attribute {
            flags: 0xef,
            code: 8,
            data: &[0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2],
        },
        Attribute {
            flags: 0xe0,
            code: 16,
            data: &[
                0, 2, 0, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 0, 0, 0, 2,
            ],
        },
        Attribute {
            flags: 0xe0,
            code: 32,
            data: &[
                0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3,
            ],
        },
        Attribute {
            flags: 0xc7,
            code: 250,
            data: &[7, 8],
        },
        Attribute {
            flags: 0x80,
            code: 249,
            data: &[255],
        },
        Attribute {
            flags: 0x40,
            code: 6,
            data: &[],
        },
        Attribute {
            flags: 0x80,
            code: 9,
            data: &[192, 0, 2, 9],
        },
        Attribute {
            flags: 0x80,
            code: 10,
            data: &[192, 0, 2, 7, 192, 0, 2, 8],
        },
    ];
    let received = receive(&mut bytes, &first, true, [192, 0, 2, 2], &[24, 203, 0, 113]);
    let a = pool.intern(&received).unwrap().unwrap();
    let mut second = first.to_vec();
    second[0].data = &[0, 0, 0, 1, 0, 0, 0, 2];
    second[0].flags = 0xe0;
    second[1].data = &[0, 2, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 0, 0, 0, 2];
    second[2].data = &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
    second[3].flags = 0xef;
    second.reverse();
    let received = receive(
        &mut bytes,
        &second,
        true,
        [192, 0, 2, 3],
        &[24, 198, 51, 100],
    );
    let b = pool.intern(&received).unwrap().unwrap();
    assert!(core::ptr::eq(a.0.get(), b.0.get()));
    assert_eq!(pool.len(), 1);
    assert_eq!(pool.path_count(), 1);
    assert_eq!(a.communities(), [1, 2]);
    assert_eq!(a.extended_communities().len(), 2);
    assert_eq!(a.large_communities().len(), 1);
    assert!(a.is_partial(8));
    assert!(a.is_partial(16));
    assert!(a.is_partial(32));
    assert_eq!(a.local_preference(), Some(100));
    assert_eq!(a.origin(), 0);
    assert!(a.atomic_aggregate());
    assert_eq!(a.originator_id(), Some(Ipv4Addr::new(192, 0, 2, 9)));
    assert_eq!(
        a.cluster_list(),
        [Ipv4Addr::new(192, 0, 2, 7), Ipv4Addr::new(192, 0, 2, 8)]
    );
    let opaque = a.unknown_transitive().collect::<Vec<_>>();
    assert_eq!(
        opaque,
        [Attribute {
            code: 250,
            flags: 0xe0,
            data: &[7, 8]
        }]
    );
    second.push(Attribute {
        flags: 0x80,
        code: 4,
        data: &[0, 0, 0, 7],
    });
    let received = receive(
        &mut bytes,
        &second,
        true,
        [192, 0, 2, 2],
        &[24, 203, 0, 113],
    );
    let different_med = pool.intern(&received).unwrap().unwrap();
    assert_ne!(a, different_med);
    assert_eq!(pool.len(), 2);
    assert!(core::ptr::eq(a.communities(), different_med.communities()));
    assert!(core::ptr::eq(
        a.extended_communities(),
        different_med.extended_communities()
    ));
    assert!(core::ptr::eq(
        a.large_communities(),
        different_med.large_communities()
    ));
    drop(different_med);
    let retained = a.clone();
    drop(a);
    drop(b);
    assert_eq!(pool.len(), 1);
    drop(retained);
    assert!(pool.is_empty());
    assert_eq!(pool.path_count(), 0);
    assert_eq!(pool.communities.len(), 0);
    assert_eq!(pool.extended_communities.len(), 0);
    assert_eq!(pool.large_communities.len(), 0);
}

#[test]
fn ordered_reflection_partial_and_missing_med_remain_distinct_and_withdrawals_do_not_intern() {
    let mut pool = AttributePool::try_new().unwrap();
    let mut bytes = vec![0; 4096];
    let mut variants = Vec::new();
    for extra in [
        vec![],
        vec![Attribute {
            flags: 0x80,
            code: 4,
            data: &[0; 4],
        }],
        vec![Attribute {
            flags: 0xc0,
            code: 8,
            data: &[0, 0, 0, 1],
        }],
        vec![Attribute {
            flags: 0xe0,
            code: 8,
            data: &[0, 0, 0, 1],
        }],
        vec![Attribute {
            flags: 0x80,
            code: 10,
            data: &[192, 0, 2, 7, 192, 0, 2, 8],
        }],
        vec![Attribute {
            flags: 0x80,
            code: 10,
            data: &[192, 0, 2, 8, 192, 0, 2, 7],
        }],
    ] {
        let received = receive(&mut bytes, &extra, true, [192, 0, 2, 2], &[24, 203, 0, 113]);
        let current = pool.intern(&received).unwrap().unwrap();
        assert!(variants.iter().all(|previous| previous != &current));
        variants.push(current);
    }
    assert_eq!(pool.len(), 6);
    assert_eq!(pool.path_count(), 1);
    assert_eq!(variants[0].med(), None);
    assert_eq!(variants[1].med(), Some(0));
    let withdraw = receive(&mut bytes, &[], true, [192, 0, 2, 2], &[]);
    assert!(pool.intern(&withdraw).unwrap().is_none());
    let malformed = receive(
        &mut bytes,
        &[Attribute {
            flags: 0xa0,
            code: 4,
            data: &[0; 4],
        }],
        true,
        [192, 0, 2, 2],
        &[24, 203, 0, 113],
    );
    assert_eq!(malformed.handling(), Handling::TreatAsWithdraw);
    assert!(pool.intern(&malformed).unwrap().is_none());
    assert_eq!(pool.len(), 6);
    drop(pool);
    assert!(variants[0].path().contains(65001));
    assert_eq!(variants[3].communities(), [1]);
}

#[test]
fn as4_and_aggregator_partial_survive_storage_and_both_peer_widths() {
    let mut pool = AttributePool::try_new().unwrap();
    for (
        base_asn,
        base_partial,
        auxiliary_partial,
        expected_path_partial,
        expected_aggregator_partial,
    ) in [
        (23456u16, false, true, true, true),
        (23456, true, false, true, true),
        (23456, false, false, true, false),
        (65010, false, true, false, false),
    ] {
        let mut aggregator = [0; 6];
        aggregator[..2].copy_from_slice(&base_asn.to_be_bytes());
        aggregator[2..].copy_from_slice(&[192, 0, 2, 7]);
        let extra = [
            Attribute {
                flags: 0xc0 | if base_partial { 0x20 } else { 0 },
                code: 7,
                data: &aggregator,
            },
            Attribute {
                flags: 0xe0,
                code: 17,
                data: &[2, 1, 0, 1, 0x11, 0x70],
            },
            Attribute {
                flags: 0xc0 | if auxiliary_partial { 0x20 } else { 0 },
                code: 18,
                data: &[0, 1, 0x11, 0x70, 192, 0, 2, 8],
            },
        ];
        let mut bytes = vec![0; 4096];
        let received = receive(
            &mut bytes,
            &extra,
            false,
            [192, 0, 2, 2],
            &[24, 203, 0, 113],
        );
        let attributes = pool.intern(&received).unwrap().unwrap();
        assert_eq!(attributes.is_partial(17), expected_path_partial);
        assert_eq!(attributes.is_partial(7), expected_aggregator_partial);
        for width in [AsnWidth::Two, AsnWidth::Four] {
            let size = attributes.emit_path_attributes(&mut bytes, width).unwrap();
            for attribute in Attributes::parse(&bytes[..size]).unwrap() {
                if matches!(attribute.code, 7 | 18) {
                    assert_eq!(attribute.flags & 0x20 != 0, expected_aggregator_partial);
                } else if attribute.code == 17 {
                    assert_eq!(attribute.flags & 0x20 != 0, expected_path_partial);
                }
            }
        }
    }
    let mut bytes = vec![0; 4096];
    let received = receive(
        &mut bytes,
        &[Attribute {
            flags: 0xe0,
            code: 17,
            data: &[2, 2, 0, 1, 0x11, 0x70, 0, 1, 0x38, 0x80],
        }],
        false,
        [192, 0, 2, 2],
        &[24, 203, 0, 113],
    );
    let attributes = pool.intern(&received).unwrap().unwrap();
    assert!(!attributes.is_partial(17));
    assert!(attributes.path().contains(65001));
}
