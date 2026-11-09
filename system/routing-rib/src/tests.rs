use super::*;
use alloc::vec;
use core::fmt::Debug;

fn source(
    id: u64,
    preference: u32,
    metric: u32,
    network: u32,
    length: u8,
    hop: u8,
) -> Arc<Source<u32, u8, u8>> {
    Source::capture(
        id,
        id,
        (),
        vec![Attributes {
            preference,
            metric,
            data: id as u8,
        }],
        vec![hop],
        vec![Route {
            network,
            length,
            attributes: 0,
            next_hop: 0,
        }],
    )
    .unwrap()
}

#[test]
fn producer_replacement_withdrawal_and_frozen_readers_preserve_selection_policy() {
    let broad = source(10, 100, 10, 0x0a00_0000, 8, 1);
    let narrow = source(20, 200, 1, 0x0a01_0200, 24, 2);
    let current = Catalogue::build(vec![narrow.clone(), broad.clone()]).unwrap();
    assert_eq!(
        current.lookup(0x0a01_0203, |_| true).unwrap().source(),
        20,
        "LPM precedes preference"
    );
    assert_eq!(
        current
            .lookup(0x0a01_0203, |route| *route.next_hop() == 1)
            .unwrap()
            .source(),
        10
    );
    let preferred = source(30, 50, 500, 0x0a00_0000, 8, 3);
    let changed = current.prepare_replace(preferred.clone()).unwrap();
    assert_eq!(changed.lookup(0x0a02_0304, |_| true).unwrap().source(), 30);
    assert_eq!(
        current.lookup(0x0a02_0304, |_| true).unwrap().source(),
        10,
        "unpublished replacement cannot modify old readers"
    );
    let equal_distance = source(30, 100, 0, 0x0a00_0000, 8, 3);
    let equal = changed.prepare_replace(equal_distance).unwrap();
    assert_eq!(
        equal.lookup(0x0a02_0304, |_| true).unwrap().source(),
        10,
        "different sources do not compare unrelated metrics"
    );
    let same_source = Source::capture(
        10,
        10,
        (),
        vec![
            Attributes {
                preference: 100,
                metric: 10,
                data: 1,
            },
            Attributes {
                preference: 100,
                metric: 5,
                data: 2,
            },
        ],
        vec![1, 4],
        vec![
            Route {
                network: 0x0a00_0000,
                length: 8,
                attributes: 0,
                next_hop: 0,
            },
            Route {
                network: 0x0a00_0000,
                length: 8,
                attributes: 1,
                next_hop: 1,
            },
        ],
    )
    .unwrap();
    let changed = equal.prepare_replace(same_source).unwrap();
    assert_eq!(
        *changed.lookup(0x0a02_0304, |_| true).unwrap().next_hop(),
        4
    );
    let removed = changed.prepare_remove(10).unwrap();
    assert_eq!(removed.lookup(0x0a02_0304, |_| true).unwrap().source(), 30);
    assert_eq!(changed.lookup(0x0a02_0304, |_| true).unwrap().source(), 10);
    let empty = Source::<u32, u8, u8>::capture(30, 30, (), vec![], vec![], vec![]).unwrap();
    let removed = removed.prepare_replace(empty).unwrap();
    assert!(removed.lookup(0x0a02_0304, |_| true).is_none());
    assert_eq!(removed.sources().len(), 1);
    assert_eq!(removed.prepare_remove(999).unwrap().len(), removed.len());
    let compact = Source::capture(
        99,
        99,
        (),
        vec![
            Attributes {
                preference: 1,
                metric: 0,
                data: 7,
            },
            Attributes {
                preference: 1,
                metric: 0,
                data: 7,
            },
            Attributes {
                preference: 2,
                metric: 1,
                data: 9,
            },
        ],
        vec![4u8, 4, 99],
        vec![
            Route {
                network: 0u32,
                length: 0,
                attributes: 0,
                next_hop: 1,
            },
            Route {
                network: 0,
                length: 0,
                attributes: 1,
                next_hop: 0,
            },
        ],
    )
    .unwrap();
    assert_eq!(compact.attributes().len(), 1);
    assert_eq!(compact.next_hops(), [4]);
    assert!(
        compact
            .routes()
            .all(|route| route.attributes == 0 && route.next_hop == 0)
    );
    assert!(matches!(
        Catalogue::build(vec![broad.clone(), broad]),
        Err(Error::InvalidSource)
    ));
    assert!(matches!(
        Source::<u32, u8, u8>::capture(0, 0, (), vec![], vec![], vec![]),
        Err(Error::InvalidSource)
    ));
    for (route, error) in [
        (
            Route {
                network: 1,
                length: 0,
                attributes: 0,
                next_hop: 0,
            },
            Error::InvalidPrefix,
        ),
        (
            Route {
                network: 0,
                length: 33,
                attributes: 0,
                next_hop: 0,
            },
            Error::InvalidPrefix,
        ),
        (
            Route {
                network: 0,
                length: 0,
                attributes: 1,
                next_hop: 0,
            },
            Error::InvalidSource,
        ),
        (
            Route {
                network: 0,
                length: 0,
                attributes: 0,
                next_hop: 1,
            },
            Error::InvalidSource,
        ),
    ] {
        assert!(
            matches!(Source::<u32, u8, u8>::capture(1, 1, (), vec![Attributes { preference: 1, metric: 1, data: 0 }], vec![0], vec![route]), Err(actual) if actual == error)
        );
    }
}

fn random(state: &mut u128) -> u128 {
    *state = state
        .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
        .wrapping_add(1);
    *state
}

fn compare_linear<A: Address + Debug>(address: fn(u128) -> A) {
    let mut state = 0x37b1_45e2_19ab_80cdu128;
    let mut sources = Vec::new();
    for id in 1..=5 {
        let attributes = (0..4)
            .map(|number| Attributes {
                preference: (id as u32 + number) % 3,
                metric: 20 - number,
                data: number,
            })
            .collect();
        let routes = (0..128)
            .map(|number| {
                let raw = random(&mut state);
                let length = (raw % (u128::from(A::BITS) + 1)) as u8;
                Route {
                    network: address(random(&mut state)).masked(length),
                    length,
                    attributes: number % 4,
                    next_hop: number % 3,
                }
            })
            .collect();
        sources.push(Source::capture(id, id, (), attributes, vec![0u8, 1, 2], routes).unwrap());
    }
    let catalogue = Catalogue::build(sources.clone()).unwrap();
    for _ in 0..512 {
        let query = address(random(&mut state));
        let expected = sources
            .iter()
            .flat_map(|source| {
                source
                    .routes
                    .iter()
                    .enumerate()
                    .map(move |(number, route)| (source, number, route))
            })
            .filter(|(source, _, route)| {
                query.masked(route.length) == route.network
                    && source.next_hops()[route.next_hop as usize] != 0
            })
            .min_by_key(|(source, number, route)| {
                let attr = &source.attributes[route.attributes as usize];
                (
                    core::cmp::Reverse(route.length),
                    attr.preference,
                    source.id(),
                    attr.metric,
                    *number,
                )
            })
            .map(|(source, number, _)| (source.id(), number as u32));
        let actual = catalogue
            .lookup(query, |candidate| *candidate.next_hop() != 0)
            .map(|candidate| {
                let source = sources
                    .iter()
                    .find(|source| source.id() == candidate.source())
                    .unwrap();
                let position = source
                    .routes
                    .iter()
                    .position(|route| core::ptr::eq(route, candidate.route()))
                    .unwrap();
                (candidate.source(), position as u32)
            });
        assert_eq!(actual, expected, "query {query:?}");
    }
    let mut linear = sources
        .iter()
        .flat_map(|source| {
            source
                .routes
                .iter()
                .enumerate()
                .map(move |(number, route)| (source, number, route))
        })
        .filter(|(source, _, route)| source.next_hops()[route.next_hop as usize] != 0)
        .collect::<Vec<_>>();
    linear.sort_by_key(|(source, number, route)| {
        let attr = &source.attributes[route.attributes as usize];
        (
            route.network,
            route.length,
            attr.preference,
            source.id(),
            attr.metric,
            *number,
        )
    });
    linear.dedup_by_key(|(_, _, route)| (route.network, route.length));
    let expected = linear
        .iter()
        .map(|(source, _, route)| (route.network, route.length, source.id()))
        .collect::<Vec<_>>();
    let actual = catalogue
        .selected(|candidate| *candidate.next_hop() != 0)
        .map(|candidate| {
            (
                candidate.route().network,
                candidate.route().length,
                candidate.source(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn catalogue_candidates_match_linear_ipv4_and_ipv6_oracles() {
    compare_linear(|value| value as u32);
    compare_linear(|value| value);
}
