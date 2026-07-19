//! Host-only AVL structure, model and deterministic-complexity tests.

use crate::fallible_tree::FallibleMap;
use core::{cell::Cell, cmp::Ordering};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    vec::Vec,
};

fn assert_model(map: &FallibleMap<i32, i64>, model: &BTreeMap<i32, i64>) {
    assert_eq!(
        map.iter()
            .map(|(&key, &value)| (key, value))
            .collect::<Vec<_>>(),
        model
            .iter()
            .map(|(&key, &value)| (key, value))
            .collect::<Vec<_>>()
    );
    map.test_assert_invariants();
}

fn insert_range(map: &mut FallibleMap<i32, i64>, start: i32, end: i32) {
    for key in start..end {
        assert_eq!(map.try_insert(key, i64::from(key)).unwrap(), None);
    }
}

#[test]
fn structural_split_preserves_order_height_length_and_node_identity() {
    let mut map = FallibleMap::new();
    insert_range(&mut map, 0, 513);
    let before = node_addresses(&map);

    let mut right = map.split_off(&173);
    assert_eq!(
        map.iter().map(|(&key, _)| key).collect::<Vec<_>>(),
        (0..173).collect::<Vec<_>>()
    );
    assert_eq!(
        right.iter().map(|(&key, _)| key).collect::<Vec<_>>(),
        (173..513).collect::<Vec<_>>()
    );
    map.test_assert_invariants();
    right.test_assert_invariants();

    let partitioned = node_addresses(&map)
        .into_iter()
        .chain(node_addresses(&right))
        .collect::<BTreeSet<_>>();
    assert_eq!(partitioned, before, "split must reuse every allocated node");

    map.append_ordered_disjoint(&mut right);
    assert!(right.is_empty());
    assert_eq!(node_addresses(&map), before);
    map.test_assert_invariants();
}

fn node_addresses<K, V>(map: &FallibleMap<K, V>) -> BTreeSet<usize> {
    let mut addresses = BTreeSet::new();
    map.test_visit_node_addresses(|address| {
        addresses.insert(address);
    });
    addresses
}

#[test]
fn split_and_join_match_btree_model_under_mixed_mutation() {
    let mut map = FallibleMap::new();
    let mut model = BTreeMap::new();
    let mut state = 0x8a5c_d789_635d_2dff_u64;

    for step in 0..4_000_i64 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let key = ((state >> 24) % 769) as i32 - 128;
        match state & 3 {
            0 | 1 => {
                assert_eq!(map.try_insert(key, step).unwrap(), model.insert(key, step));
            }
            2 => assert_eq!(map.remove(&key), model.remove(&key)),
            _ => {
                let mut right = map.split_off(&key);
                let mut right_model = model.split_off(&key);
                assert_model(&map, &model);
                assert_model(&right, &right_model);
                map.append_ordered_disjoint(&mut right);
                model.append(&mut right_model);
                assert!(right.is_empty());
            }
        }
        assert_model(&map, &model);
    }
}

#[test]
fn ordered_join_fail_stops_before_mutating_either_map() {
    for (left_keys, right_keys) in [([1, 3], [3, 4]), ([5, 7], [1, 2])] {
        let mut left = FallibleMap::new();
        let mut right = FallibleMap::new();
        for key in left_keys {
            left.try_insert(key, key).unwrap();
        }
        for key in right_keys {
            right.try_insert(key, key).unwrap();
        }
        let left_before = left
            .iter()
            .map(|(&key, &value)| (key, value))
            .collect::<Vec<_>>();
        let right_before = right
            .iter()
            .map(|(&key, &value)| (key, value))
            .collect::<Vec<_>>();

        let result = catch_unwind(AssertUnwindSafe(|| {
            left.append_ordered_disjoint(&mut right);
        }));
        assert!(result.is_err());
        assert_eq!(
            left.iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>(),
            left_before
        );
        assert_eq!(
            right
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>(),
            right_before
        );
        left.test_assert_invariants();
        right.test_assert_invariants();
    }
}

#[derive(Clone, Debug)]
struct CountingKey {
    value: usize,
    comparisons: Rc<Cell<usize>>,
}

impl PartialEq for CountingKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for CountingKey {}

impl PartialOrd for CountingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CountingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.comparisons.set(self.comparisons.get() + 1);
        self.value.cmp(&other.value)
    }
}

#[test]
fn split_and_join_have_stable_key_comparison_bounds() {
    const ENTRIES: usize = 16_384;
    let comparisons = Rc::new(Cell::new(0));
    let key = |value| CountingKey {
        value,
        comparisons: comparisons.clone(),
    };
    let mut map = FallibleMap::new();
    for value in 0..ENTRIES {
        map.try_insert(key(value), value).unwrap();
    }
    let original_height = usize::from(map.test_root_height());

    comparisons.set(0);
    let mut right = map.split_off(&key(1));
    let split_comparisons = comparisons.get();
    assert!(
        split_comparisons <= original_height,
        "split compared {split_comparisons} keys for AVL height {original_height}"
    );
    assert_eq!(map.len(), 1);
    assert_eq!(right.len(), ENTRIES - 1);

    comparisons.set(0);
    map.append_ordered_disjoint(&mut right);
    assert_eq!(
        comparisons.get(),
        1,
        "ordered join needs one boundary comparison"
    );
    assert_eq!(map.len(), ENTRIES);
    assert!(right.is_empty());
    map.test_assert_invariants();
}

#[test]
fn ceiling_and_successor_match_ordered_neighbors_with_logarithmic_comparisons() {
    const ENTRIES: usize = 16_384;
    let comparisons = Rc::new(Cell::new(0));
    let key = |value| CountingKey {
        value,
        comparisons: comparisons.clone(),
    };
    let mut map = FallibleMap::new();
    for value in (0..ENTRIES).step_by(2) {
        map.try_insert(key(value), value).unwrap();
    }
    let height = usize::from(map.test_root_height());

    for (query, ceiling, successor) in [
        (0, Some(0), Some(2)),
        (1, Some(2), Some(2)),
        (ENTRIES - 2, Some(ENTRIES - 2), None),
        (ENTRIES - 1, None, None),
    ] {
        comparisons.set(0);
        assert_eq!(map.ceiling(&key(query)).map(|(_, value)| *value), ceiling);
        assert!(comparisons.get() <= height + 1);
        comparisons.set(0);
        assert_eq!(
            map.successor(&key(query)).map(|(_, value)| *value),
            successor
        );
        assert!(comparisons.get() <= height + 1);
    }
}

#[test]
fn ordered_scan_has_compact_stack_and_linear_comparison_budget() {
    const ENTRIES: usize = 4_096;
    let comparisons = Rc::new(Cell::new(0));
    let key = |value| CountingKey {
        value,
        comparisons: comparisons.clone(),
    };
    let mut map = FallibleMap::new();
    for value in 0..ENTRIES {
        map.try_insert(key(value), value).unwrap();
    }

    comparisons.set(0);
    let mut visited = 0;
    for (expected, (_, value)) in map.iter().enumerate() {
        assert_eq!(*value, expected);
        visited += 1;
    }
    let iterator_bytes = core::mem::size_of_val(&map.iter());
    let lookup_comparisons = comparisons.get();
    assert!(
        visited == ENTRIES && iterator_bytes <= 16 && lookup_comparisons <= 1,
        "ordered scan visited {visited} entries with a {iterator_bytes}-byte iterator frame and {lookup_comparisons} key comparisons"
    );
}

#[test]
fn retain_matches_random_btree_models_without_replacing_kept_nodes() {
    let mut map = FallibleMap::new();
    let mut model = BTreeMap::new();
    let mut state = 0x243f_6a88_85a3_08d3_u64;

    for round in 0..64_i64 {
        for _ in 0..96 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = ((state >> 19) % 2_049) as i32 - 1_024;
            let value = round ^ i64::from(key);
            assert_eq!(
                map.try_insert(key, value).unwrap(),
                model.insert(key, value)
            );
        }
        let before = node_addresses(&map);
        let salt = (state >> 32) as i32;
        map.retain(|key, value| (key.wrapping_mul(31) ^ (*value as i32) ^ salt) & 3 != 0);
        model.retain(|key, value| (key.wrapping_mul(31) ^ (*value as i32) ^ salt) & 3 != 0);
        assert_model(&map, &model);
        assert!(
            node_addresses(&map).is_subset(&before),
            "retain must reuse kept node ownership"
        );
    }
}

#[test]
fn retain_calls_predicate_once_per_node_without_key_comparisons() {
    const ENTRIES: usize = 4_096;
    let comparisons = Rc::new(Cell::new(0));
    let key = |value| CountingKey {
        value,
        comparisons: comparisons.clone(),
    };
    let mut map = FallibleMap::new();
    for value in 0..ENTRIES {
        map.try_insert(key(value), value).unwrap();
    }
    let before = node_addresses(&map);
    let visits = Cell::new(0);

    comparisons.set(0);
    map.retain(|_, _| {
        visits.set(visits.get() + 1);
        true
    });

    assert_eq!(visits.get(), ENTRIES);
    assert_eq!(comparisons.get(), 0);
    assert_eq!(node_addresses(&map), before);
    map.test_assert_invariants();
}
