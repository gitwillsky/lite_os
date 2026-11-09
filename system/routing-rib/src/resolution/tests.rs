use super::*;
use alloc::vec;
use router_protocol::routing::{self, Batch, Family};

fn capture_table(
    rows: &[routing::Route],
    hops: &[routing::NextHop<'_>],
) -> Arc<Catalogue<u32, (), super::super::NextHop>> {
    let bytes = Batch::encode(
        Family::Ipv4,
        &[routing::Attributes {
            preference: 1,
            metric: 0,
        }],
        hops,
        rows,
    )
    .unwrap();
    let source = Source::<u32, (), super::super::NextHop>::capture_batch(
        1,
        1,
        (),
        Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    Catalogue::build(vec![source]).unwrap()
}
fn row(network: &str, length: u8, next_hop: u32) -> routing::Route {
    routing::Route {
        network: network.parse().unwrap(),
        length,
        attributes: 0,
        next_hop,
    }
}
fn direct(interface_index: u32, weight: u32) -> routing::Member {
    routing::Member {
        target: Target::Direct {
            interface_index,
            gateway: None,
            on_link: false,
        },
        weight,
    }
}
fn recursive(address: &str, weight: u32) -> routing::Member {
    routing::Member {
        target: Target::Recursive(address.parse().unwrap()),
        weight,
    }
}
fn lookup<'a>(table: &'a Resolved<u32, ()>, address: &str) -> Selected<'a, u32, ()> {
    table
        .lookup(u32::from(address.parse::<core::net::Ipv4Addr>().unwrap()))
        .unwrap()
}
#[test]
fn recursive_chains_flatten_exact_weights_and_keep_frozen_readers() {
    let leaf = [direct(2, 1), direct(3, 3)];
    let middle = [recursive("192.0.2.1", 2), direct(4, 1)];
    let root = [recursive("198.51.100.1", 1)];
    let hops = [
        routing::NextHop::new(Action::Forward, &leaf),
        routing::NextHop::new(Action::Forward, &middle),
        routing::NextHop::new(Action::Forward, &root),
    ];
    let table = capture_table(
        &[
            row("192.0.2.0", 24, 0),
            row("198.51.100.0", 24, 1),
            row("203.0.113.0", 24, 2),
        ],
        &hops,
    );
    let compiled = table.resolve(|_| true, |_| true).unwrap();
    assert_eq!(
        lookup(&compiled, "198.51.100.9")
            .next_hop
            .unwrap()
            .members()
            .iter()
            .map(|member| member.weight)
            .collect::<Vec<_>>(),
        [1, 3, 2],
        "every intermediate group must already be in lowest integer terms"
    );
    let resolved = lookup(&compiled, "203.0.113.9");
    let members = resolved.next_hop.unwrap().members();
    assert_eq!(
        members
            .iter()
            .map(|member| (member.target.interface_index, member.weight))
            .collect::<Vec<_>>(),
        [(2, 1), (3, 3), (4, 2)]
    );
    assert_eq!(
        members[0].target.gateway,
        Some("192.0.2.1".parse().unwrap())
    );
    assert_eq!(
        members[2].target.gateway,
        Some("198.51.100.1".parse().unwrap())
    );
    assert!(!resolved.issues.cycle && !resolved.issues.unreachable);
    let contextual = table
        .resolve(
            |_| true,
            |target| target.interface_index != 3 || target.gateway.is_none(),
        )
        .unwrap();
    assert_eq!(
        lookup(&contextual, "203.0.113.9")
            .next_hop
            .unwrap()
            .members()
            .iter()
            .map(|member| (member.target.interface_index, member.weight))
            .collect::<Vec<_>>(),
        [(2, 2), (4, 1)]
    );
    let changed = table
        .resolve(|_| true, |target| target.interface_index != 3)
        .unwrap();
    let changed = lookup(&changed, "203.0.113.9");
    assert_eq!(
        changed
            .next_hop
            .unwrap()
            .members()
            .iter()
            .map(|member| member.target.interface_index)
            .collect::<Vec<_>>(),
        [2, 4]
    );
    assert!(changed.issues.unreachable);
    assert_eq!(
        lookup(&compiled, "203.0.113.9")
            .next_hop
            .unwrap()
            .members()
            .len(),
        3
    );
    let wide_leaf = [direct(2, 1), direct(3, u32::MAX)];
    let wide_root = [recursive("192.0.2.1", 1), direct(4, 1)];
    let overflow = capture_table(
        &[row("192.0.2.0", 24, 0), row("198.51.100.0", 24, 1)],
        &[
            routing::NextHop::new(Action::Forward, &wide_leaf),
            routing::NextHop::new(Action::Forward, &wide_root),
        ],
    );
    assert!(
        matches!(overflow.resolve(|_| true, |_| true), Err(Error::Capacity)),
        "exact weights need [1, u32::MAX, 2^32]; do not truncate"
    );
    let immediate = routing::Member {
        target: Target::Direct {
            interface_index: 2,
            gateway: Some("192.0.2.1".parse().unwrap()),
            on_link: false,
        },
        weight: 1,
    };
    let converging = [recursive("192.0.2.1", 1), immediate];
    let leaf = [direct(2, 1)];
    let merged = capture_table(
        &[row("192.0.2.0", 24, 0), row("198.51.100.0", 24, 1)],
        &[
            routing::NextHop::new(Action::Forward, &leaf),
            routing::NextHop::new(Action::Forward, &converging),
        ],
    );
    let merged = merged.resolve(|_| true, |_| true).unwrap();
    let merged = lookup(&merged, "198.51.100.9");
    assert_eq!(merged.next_hop.unwrap().members().len(), 1);
    assert_eq!(merged.next_hop.unwrap().members()[0].weight, 1);
}
#[test]
fn mutual_recursion_does_not_poison_another_root_or_bypass_a_selected_drop() {
    let a = [recursive("198.51.100.1", 1), direct(2, 1)];
    let b = [recursive("192.0.2.1", 1), direct(3, 1)];
    let cycle = [recursive("203.0.113.1", 1)];
    let via_drop = [recursive("10.0.0.1", 1)];
    let parent = [direct(9, 1)];
    let hops = [
        routing::NextHop::new(Action::Forward, &a),
        routing::NextHop::new(Action::Forward, &b),
        routing::NextHop::new(Action::Forward, &cycle),
        routing::NextHop::new(Action::Forward, &via_drop),
        routing::NextHop::new(Action::Discard, &[]),
        routing::NextHop::new(Action::Forward, &parent),
    ];
    let table = capture_table(
        &[
            row("192.0.2.0", 24, 0),
            row("198.51.100.0", 24, 1),
            row("203.0.113.0", 24, 2),
            row("172.16.0.0", 16, 3),
            row("10.0.0.0", 8, 4),
            row("0.0.0.0", 0, 5),
        ],
        &hops,
    );
    let result = table.resolve(|_| true, |_| true).unwrap();
    assert_eq!(
        result.group_count(),
        4,
        "context-only partial groups must retire"
    );
    for address in ["192.0.2.9", "198.51.100.9"] {
        let selected = lookup(&result, address);
        assert!(selected.issues.cycle);
        assert_eq!(
            selected
                .next_hop
                .unwrap()
                .members()
                .iter()
                .map(|member| member.target.interface_index)
                .collect::<Vec<_>>(),
            [2, 3]
        );
    }
    let self_cycle = lookup(&result, "203.0.113.9");
    assert!(self_cycle.next_hop.is_none() && self_cycle.issues.cycle);
    assert_eq!(
        lookup(&result, "10.0.0.1").next_hop.unwrap().action(),
        Action::Discard
    );
    let dropped = lookup(&result, "172.16.0.1");
    assert!(dropped.next_hop.is_none() && dropped.issues.unreachable);
}

#[test]
fn deep_dual_stack_chains_use_the_explicit_work_stack_and_share_final_groups() {
    for family in [Family::Ipv4, Family::Ipv6] {
        let addresses: Vec<IpAddr> = (0..2048)
            .map(|index| match family {
                Family::Ipv4 => core::net::Ipv4Addr::from(0xc612_0000 + index).into(),
                Family::Ipv6 => core::net::Ipv6Addr::from(
                    0x2001_0db8_0000_0000_0000_0000_0000_0000u128 + u128::from(index),
                )
                .into(),
            })
            .collect();
        let members: Vec<_> = addresses
            .iter()
            .skip(1)
            .map(|address| {
                vec![routing::Member {
                    target: Target::Recursive(*address),
                    weight: 1,
                }]
            })
            .chain(core::iter::once(vec![direct(2, 1)]))
            .collect();
        let hops: Vec<_> = members
            .iter()
            .map(|members| routing::NextHop::new(Action::Forward, members))
            .collect();
        let rows: Vec<_> = addresses
            .iter()
            .enumerate()
            .map(|(index, address)| routing::Route {
                network: *address,
                length: if family == Family::Ipv4 { 32 } else { 128 },
                attributes: 0,
                next_hop: index as u32,
            })
            .collect();
        let bytes = Batch::encode(
            family,
            &[routing::Attributes {
                preference: 1,
                metric: 0,
            }],
            &hops,
            &rows,
        )
        .unwrap();
        let batch = Batch::parse(&bytes).unwrap();
        let (groups, gateway) = match addresses[0] {
            IpAddr::V4(address) => {
                let source =
                    Source::<u32, (), super::super::NextHop>::capture_batch(1, 1, (), batch)
                        .unwrap();
                let table = Catalogue::build(vec![source])
                    .unwrap()
                    .resolve(|_| true, |_| true)
                    .unwrap();
                (
                    table.group_count(),
                    table
                        .lookup(u32::from(address))
                        .unwrap()
                        .next_hop
                        .unwrap()
                        .members()[0]
                        .target
                        .gateway,
                )
            }
            IpAddr::V6(address) => {
                let source =
                    Source::<u128, (), super::super::NextHop>::capture_batch(1, 1, (), batch)
                        .unwrap();
                let table = Catalogue::build(vec![source])
                    .unwrap()
                    .resolve(|_| true, |_| true)
                    .unwrap();
                (
                    table.group_count(),
                    table
                        .lookup(u128::from(address))
                        .unwrap()
                        .next_hop
                        .unwrap()
                        .members()[0]
                        .target
                        .gateway,
                )
            }
        };
        assert_eq!(gateway, addresses.last().copied());
        assert_eq!(
            groups, 2,
            "all recursive prefixes share the final immediate gateway"
        );
    }
}

#[test]
fn reselection_restores_a_recursive_peer_after_choosing_a_loop_breaking_backup() {
    let a = [recursive("198.51.100.1", 1)];
    let b = [recursive("192.0.2.1", 1)];
    let backup = [direct(2, 1)];
    let table = capture_table(
        &[
            row("192.0.2.0", 24, 0),
            row("192.0.2.0", 24, 2),
            row("198.51.100.0", 24, 1),
        ],
        &[
            routing::NextHop::new(Action::Forward, &a),
            routing::NextHop::new(Action::Forward, &b),
            routing::NextHop::new(Action::Forward, &backup),
        ],
    );
    let frozen = table.resolve(|_| true, |_| true).unwrap();
    assert!(lookup(&frozen, "192.0.2.9").next_hop.is_none());
    assert!(lookup(&frozen, "198.51.100.9").next_hop.is_none());
    let selected = table.select_resolved(|_| true, |_| true).unwrap();
    let a = lookup(&selected, "192.0.2.9");
    let b = lookup(&selected, "198.51.100.9");
    assert!(matches!(
        a.candidate.next_hop().members()[0].target,
        Target::Direct {
            interface_index: 2,
            ..
        }
    ));
    assert!(
        matches!(
            b.candidate.next_hop().members()[0].target,
            Target::Recursive(_)
        ),
        "bulk rejection must not permanently discard the recoverable peer"
    );
    assert_eq!(
        b.next_hop.unwrap().members()[0].target.gateway,
        Some("192.0.2.1".parse().unwrap())
    );
    assert!(selected.selected().all(|row| row.next_hop.is_some()));
    assert!(
        lookup(&frozen, "192.0.2.9").next_hop.is_none(),
        "selection cannot change a frozen reader"
    );
}

#[test]
fn reselection_withdraws_self_capture_without_bypassing_a_selected_drop() {
    let parent = [direct(7, 1)];
    let blocked = [recursive("10.0.0.1", 1)];
    let backup = [direct(8, 1)];
    let recursive = [recursive("192.0.2.1", 1)];
    let table = capture_table(
        &[
            row("0.0.0.0", 0, 0),
            row("10.0.0.0", 8, 1),
            row("172.16.0.0", 16, 2),
            row("172.16.0.0", 16, 3),
            row("192.0.2.0", 24, 4),
            row("198.51.100.0", 24, 4),
        ],
        &[
            routing::NextHop::new(Action::Forward, &parent),
            routing::NextHop::new(Action::Discard, &[]),
            routing::NextHop::new(Action::Forward, &blocked),
            routing::NextHop::new(Action::Forward, &backup),
            routing::NextHop::new(Action::Forward, &recursive),
        ],
    );
    let selected = table.select_resolved(|_| true, |_| true).unwrap();
    assert_eq!(
        lookup(&selected, "10.0.0.1").next_hop.unwrap().action(),
        Action::Discard
    );
    assert_eq!(
        lookup(&selected, "172.16.1.1").next_hop.unwrap().members()[0]
            .target
            .interface_index,
        8
    );
    assert_eq!(
        lookup(&selected, "192.0.2.9").candidate.route().length,
        0,
        "a self-capturing route must not validate itself through the older parent"
    );
    let recovered = lookup(&selected, "198.51.100.9");
    assert_eq!(recovered.candidate.route().length, 24);
    assert_eq!(
        recovered.next_hop.unwrap().members()[0].target.gateway,
        Some("192.0.2.1".parse().unwrap())
    );
}

#[test]
fn source_withdrawal_and_interface_recovery_recompute_choices_without_stale_exclusions() {
    let attached = [direct(2, 1)];
    let first = capture_table(
        &[row("192.0.2.0", 24, 0)],
        &[routing::NextHop::new(Action::Forward, &attached)],
    );
    let first = first.sources()[0].clone();
    let recursive = [recursive("192.0.2.1", 1)];
    let backup = [direct(3, 1)];
    let bytes = Batch::encode(
        Family::Ipv4,
        &[routing::Attributes {
            preference: 1,
            metric: 0,
        }],
        &[
            routing::NextHop::new(Action::Forward, &recursive),
            routing::NextHop::new(Action::Forward, &backup),
        ],
        &[row("198.51.100.0", 24, 0), row("198.51.100.0", 24, 1)],
    )
    .unwrap();
    let second = Source::<u32, (), super::super::NextHop>::capture_batch(
        2,
        2,
        (),
        Batch::parse(&bytes).unwrap(),
    )
    .unwrap();
    let table = Catalogue::build(vec![first.clone(), second]).unwrap();
    let initial = table.select_resolved(|_| true, |_| true).unwrap();
    let destination = "198.51.100.9";
    assert_eq!(
        lookup(&initial, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        2
    );
    let removed = table.prepare_remove(1).unwrap();
    let after = removed.select_resolved(|_| true, |_| true).unwrap();
    assert_eq!(
        lookup(&after, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        3
    );
    let down = table
        .select_resolved(|_| true, |target| target.interface_index != 2)
        .unwrap();
    assert_eq!(
        lookup(&down, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        3
    );
    let restored = removed
        .prepare_replace(first)
        .unwrap()
        .select_resolved(|_| true, |_| true)
        .unwrap();
    assert_eq!(
        lookup(&restored, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        2
    );
    assert_eq!(
        lookup(&initial, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        2
    );
    assert_eq!(
        lookup(&down, destination).next_hop.unwrap().members()[0]
            .target
            .interface_index,
        3
    );
}

#[test]
fn shadowed_aggregate_failures_do_not_trigger_a_full_graph_trial_per_prefix() {
    // 所有聚合覆盖递归地址，但稳定 /32 已遮住它们。仅检查解析结果不能发现错误分类造成的平方级重编译。
    const COUNT: u8 = 64;
    let mut members = vec![vec![direct(3, 1)], vec![direct(2, 1)]];
    let mut rows = Vec::new();
    for index in 0..COUNT {
        let network = core::net::Ipv4Addr::new(10, index, 0, 0);
        let target = core::net::Ipv4Addr::new(10, index, 0, 1);
        let destination = core::net::Ipv4Addr::new(198, 18, index, 0);
        let next_hop = members.len() as u32;
        members.push(vec![routing::Member {
            target: Target::Recursive(target.into()),
            weight: 1,
        }]);
        for (network, length, next_hop) in [
            (network, 24, 0),
            (network, 24, 1),
            (target, 32, 1),
            (destination, 24, next_hop),
        ] {
            rows.push(routing::Route {
                network: network.into(),
                length,
                attributes: 0,
                next_hop,
            });
        }
    }
    let hops = members
        .iter()
        .map(|members| routing::NextHop::new(Action::Forward, members))
        .collect::<Vec<_>>();
    let table = capture_table(&rows, &hops);
    let visits = core::cell::Cell::new(0);
    let selected = table
        .select_resolved(
            |_| {
                visits.set(visits.get() + 1);
                true
            },
            |target| target.interface_index != 3,
        )
        .unwrap();
    // 三次选择扫描加一次候选扫描足够；逐聚合完整求解会随前缀数平方增长，远超这个线性工作量。
    assert!(visits.get() <= 4 * rows.len(), "{} visits", visits.get());
    assert_eq!(selected.selected().count(), usize::from(COUNT) * 3);
    for index in 0..COUNT {
        let route = selected
            .lookup(u32::from(core::net::Ipv4Addr::new(198, 18, index, 9)))
            .unwrap();
        assert_eq!(
            route.next_hop.unwrap().members()[0].target.gateway,
            Some(core::net::Ipv4Addr::new(10, index, 0, 1).into())
        );
    }
}
