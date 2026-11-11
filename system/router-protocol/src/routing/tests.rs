use super::*;
use alloc::vec;

fn route(network: IpAddr, length: u8, next_hop: u32) -> Route {
    Route {
        network,
        length,
        attributes: 0,
        next_hop,
    }
}
#[test]
fn dual_stack_sources_borrow_shared_objects_and_preserve_recursive_targets() {
    for (family, network, gateway, length) in [
        (Family::Ipv4, "192.0.2.0", "192.0.2.1", 24),
        (Family::Ipv6, "2001:db8::", "2001:db8::1", 64),
    ] {
        let network: IpAddr = network.parse().unwrap();
        let gateway: IpAddr = gateway.parse().unwrap();
        let members = [
            Member {
                target: Target::Direct {
                    interface_index: 7,
                    gateway: None,
                    on_link: false,
                },
                weight: 2,
            },
            Member {
                target: Target::Recursive(gateway),
                weight: 3,
            },
        ];
        let hops = [
            NextHop::new(Action::Forward, &members),
            NextHop::new(Action::Discard, &[]),
            NextHop::new(Action::Unreachable, &[]),
            NextHop::new(Action::Prohibit, &[]),
        ];
        let attributes = [Attributes {
            preference: 110,
            metric: 20,
        }];
        let rows = [route(network, length, 0), route(family.zero(), 0, 1)];
        let bytes = Batch::encode(family, &attributes, hops, rows).unwrap();
        let batch = Batch::parse(&bytes).unwrap();
        assert_eq!(batch.family(), family);
        assert!(batch.attributes().eq(attributes));
        assert!(batch.routes().eq(rows));
        let mut decoded = batch.next_hops();
        for original in hops {
            let read = decoded.next().unwrap();
            assert_eq!(read.action(), original.action());
            assert_eq!(read.len(), original.len());
            assert!(read.members().eq(original.members()));
        }
        assert!(decoded.next().is_none());
        let direct = [Member {
            target: Target::Direct {
                interface_index: 7,
                gateway: Some(gateway),
                on_link: false,
            },
            weight: 1,
        }];
        let direct = Batch::encode(
            family,
            &attributes,
            [NextHop::new(Action::Forward, &direct)],
            core::iter::empty::<Route>(),
        )
        .unwrap();
        let borrowed = Batch::parse(&direct).unwrap().next_hops().next().unwrap();
        let other = match family {
            Family::Ipv4 => Family::Ipv6,
            Family::Ipv6 => Family::Ipv4,
        };
        assert_eq!(
            borrowed.validate(other),
            Err(Invalid),
            "encoded member width cannot change during preflight"
        );
        let empty = Batch::encode(
            family,
            &[],
            core::iter::empty::<NextHop<'_>>(),
            core::iter::empty::<Route>(),
        )
        .unwrap();
        let empty = Batch::parse(&empty).unwrap();
        assert_eq!(empty.routes().len(), 0);
        assert_eq!(empty.next_hops().len(), 0);
    }
}
#[test]
fn malformed_sources_fail_before_exposing_unchecked_dictionary_views() {
    let family = Family::Ipv4;
    let attributes = [Attributes {
        preference: 1,
        metric: 0,
    }];
    let members = [Member {
        target: Target::Direct {
            interface_index: 7,
            gateway: None,
            on_link: false,
        },
        weight: 1,
    }];
    let hops = [NextHop::new(Action::Forward, &members)];
    let rows = [route("192.0.2.0".parse().unwrap(), 24, 0)];
    let bytes = Batch::encode(family, &attributes, hops, rows).unwrap();
    for end in 0..bytes.len() {
        assert!(Batch::parse(&bytes[..end]).is_err());
    }
    let mut tail = bytes.clone();
    tail.push(0);
    assert!(Batch::parse(&tail).is_err());
    // 20-byte header, 8-byte attribute, 5-byte group header, 26-byte member.
    for (offset, value) in [
        (0, 99),
        (4, 5),
        (28, 9),
        (33, 0),
        (37, 9),
        (38, 0),
        (42, 4),
        (43, 1),
        (51, 1),
        (63, 33),
        (64, 1),
        (68, 1),
    ] {
        let mut corrupt = bytes.clone();
        corrupt[offset] = value;
        assert!(Batch::parse(&corrupt).is_err(), "offset {offset}");
    }
    for offset in [8, 12, 16] {
        let mut corrupt = bytes.clone();
        corrupt[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Batch::parse(&corrupt).is_err());
    }
    for bad in [
        route("192.0.2.1".parse().unwrap(), 24, 0),
        route("::".parse().unwrap(), 0, 0),
        Route {
            attributes: 1,
            ..rows[0]
        },
        Route {
            next_hop: 1,
            ..rows[0]
        },
    ] {
        assert!(Batch::encode(family, &attributes, hops, [bad]).is_err());
    }
    for member in [
        Member {
            weight: 0,
            ..members[0]
        },
        Member {
            target: Target::Direct {
                interface_index: 0,
                gateway: None,
                on_link: false,
            },
            weight: 1,
        },
        Member {
            target: Target::Direct {
                interface_index: 7,
                gateway: None,
                on_link: true,
            },
            weight: 1,
        },
        Member {
            target: Target::Recursive("0.0.0.0".parse().unwrap()),
            weight: 1,
        },
        Member {
            target: Target::Recursive("224.0.0.1".parse().unwrap()),
            weight: 1,
        },
        Member {
            target: Target::Recursive("::1".parse().unwrap()),
            weight: 1,
        },
    ] {
        assert!(
            Batch::encode(
                family,
                &attributes,
                [NextHop::new(Action::Forward, &[member])],
                rows
            )
            .is_err()
        );
    }
    assert!(
        Batch::encode(
            Family::Ipv6,
            &attributes,
            [NextHop::new(
                Action::Forward,
                &[Member {
                    target: Target::Recursive("fe80::1".parse().unwrap()),
                    weight: 1
                }]
            )],
            core::iter::empty::<Route>()
        )
        .is_err()
    );
    for hop in [
        NextHop::new(Action::Forward, &[]),
        NextHop::new(Action::Discard, &members),
    ] {
        assert!(Batch::encode(family, &attributes, [hop], rows).is_err());
    }
    let terminal = vec![NextHop::new(Action::Discard, &[])];
    assert!(Batch::encode(family, &attributes, &terminal, rows).is_ok());
    // Clone 可共享外部状态；错误的重放必须返回 Invalid，不能写出数量不符的正文或隐式扩容。
    let cursor = core::cell::Cell::new(0);
    let disappearing = core::iter::from_fn(|| {
        let current = cursor.get();
        cursor.set(current + 1);
        (current == 0).then_some(rows[0])
    });
    assert_eq!(
        Batch::encode(family, &attributes, hops, disappearing),
        Err(EncodeError::Invalid)
    );
    let cursor = core::cell::Cell::new(0);
    let appearing = core::iter::from_fn(|| {
        let current = cursor.get();
        cursor.set(current + 1);
        (current == 1).then_some(hops[0])
    });
    assert_eq!(
        Batch::encode(family, &attributes, appearing, core::iter::empty::<Route>()),
        Err(EncodeError::Invalid)
    );
}
