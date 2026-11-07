#[cfg(test)]
extern crate alloc;

#[cfg(test)]
#[path = "../../../kernel/src/thread/scheduler/preallocated_heap.rs"]
mod preallocated_heap;

#[cfg(test)]
#[path = "../../../kernel/src/thread/scheduler/preemption_policy.rs"]
mod preemption_policy;

#[cfg(test)]
#[path = "../../../kernel/src/thread/scheduler/placement_policy.rs"]
mod placement_policy;

#[cfg(test)]
#[path = "../../../system/linuxd/src/task/process_table/signal/selection_result.rs"]
mod signal_selection_result;

#[cfg(test)]
mod native_scheduling;

#[cfg(test)]
use native_scheduling::{arch, execution};

#[cfg(test)]
mod tests {
    use super::preallocated_heap::PreallocatedHeap;

    #[test]
    fn spare_capacity_never_invokes_the_compaction_predicate() {
        let mut heap = PreallocatedHeap::try_with_capacity(64).unwrap();
        let reserved = heap.capacity();
        let mut checks = 0;

        for value in 0..64 {
            assert_eq!(
                heap.make_room(1, |_| {
                    checks += 1;
                    true
                }),
                0
            );
            heap.push(value);
        }

        assert_eq!(checks, 0);
        assert_eq!(heap.len(), 64);
        assert_eq!(heap.capacity(), reserved);
    }

    #[test]
    fn capacity_pressure_compacts_once_and_preserves_heap_order() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in 0..8 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let reserved = heap.capacity();
        let mut checks = 0;

        let removed = heap.make_room(1, |value| {
            checks += 1;
            value % 2 == 0
        });
        heap.push(9);

        assert_eq!(removed, 4);
        assert_eq!(checks, 8);
        assert_eq!(heap.capacity(), reserved);
        let mut popped = alloc::vec::Vec::new();
        while let Some(value) = heap.pop() {
            popped.push(value);
        }
        assert_eq!(popped, [9, 6, 4, 2, 0]);
    }

    #[test]
    fn batch_pressure_compacts_and_can_adopt_larger_backing() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in 0..8 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let reserved = heap.capacity();
        let mut checks = 0;

        assert_eq!(
            heap.make_room(4, |value| {
                checks += 1;
                value % 2 == 0
            }),
            4
        );
        for value in 8..12 {
            heap.push(value);
        }

        assert_eq!(checks, 8);
        assert_eq!(heap.len(), 8);
        assert_eq!(heap.capacity(), reserved);
        let spare = PreallocatedHeap::try_with_capacity(16).unwrap();
        let new_capacity = spare.capacity();
        heap.grow(spare);
        assert_eq!(heap.capacity(), new_capacity);
        assert!(heap.has_room(1));
        heap.push(12);
        let mut popped = alloc::vec::Vec::new();
        while let Some(value) = heap.pop() {
            popped.push(value);
        }
        assert_eq!(popped, [12, 11, 10, 9, 8, 6, 4, 2, 0]);
    }

    #[test]
    #[should_panic(expected = "preallocated heap capacity exhausted by live entries")]
    fn live_capacity_divergence_is_fail_stop() {
        let mut heap = PreallocatedHeap::try_with_capacity(4).unwrap();
        for value in 0..4 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }

        heap.make_room(1, |_| true);
    }

    #[test]
    fn root_pruning_stops_at_the_first_live_heap_root() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in [10, 20, 40, 50, 99, 100] {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let mut checks = 0;

        let removed = heap.discard_invalid_roots(|value| {
            checks += 1;
            *value <= 50
        });

        assert_eq!(removed, 2);
        assert_eq!(checks, 3);
        assert_eq!(heap.peek(), Some(&50));
    }
}

#[cfg(test)]
mod signal_selection_tests {
    use super::signal_selection_result::{SelectionAttempt, SelectionOutcome, SelectionResult};

    #[test]
    fn success_is_sticky_across_denials() {
        for attempts in [
            [SelectionAttempt::Generated, SelectionAttempt::Denied],
            [SelectionAttempt::Denied, SelectionAttempt::Generated],
        ] {
            let mut result = SelectionResult::new();
            for attempt in attempts {
                result.record(attempt);
            }
            assert_eq!(result.finish(), SelectionOutcome::Success(1));
        }
    }

    #[test]
    fn denial_is_distinct_from_no_candidate() {
        let mut denied = SelectionResult::new();
        denied.record(SelectionAttempt::Denied);

        assert_eq!(denied.finish(), SelectionOutcome::Permission);
        assert_eq!(SelectionResult::new().finish(), SelectionOutcome::NotFound);
    }

    #[test]
    fn signal_zero_probe_succeeds_without_generation() {
        let mut result = SelectionResult::new();
        result.record(SelectionAttempt::Probe);

        assert_eq!(result.finish(), SelectionOutcome::Success(1));
    }

    #[test]
    fn kernel_group_delivery_counts_every_completed_generation() {
        let mut result = SelectionResult::new();
        for _ in 0..4 {
            result.record(SelectionAttempt::Generated);
        }

        assert_eq!(result.finish(), SelectionOutcome::Success(4));
    }
}

#[cfg(test)]
mod preemption_policy_tests {
    use super::preemption_policy::local_ready_preempts;

    #[test]
    fn only_an_earlier_live_ready_root_preempts() {
        assert!(local_ready_preempts(Some(20_000), Some(1_000)));
        assert!(!local_ready_preempts(Some(10_000), Some(1_000_000)));
        assert!(!local_ready_preempts(Some(50_000), Some(50_000)));
    }

    #[test]
    fn stale_or_idle_delivery_does_not_fabricate_policy_preemption() {
        assert!(!local_ready_preempts(Some(20_000), None));
        assert!(!local_ready_preempts(None, Some(1_000)));
    }
}

#[cfg(test)]
mod placement_policy_tests {
    use super::placement_policy::{
        ROTATION_INTERVAL_PREEMPTIONS, advance_rotation, can_rotate, choose_cpu,
    };

    fn choose(candidates: &[(usize, usize)], last: Option<usize>, rotate: bool) -> Option<usize> {
        choose_cpu(candidates.iter().copied(), last, rotate)
    }

    #[test]
    fn no_candidate_means_no_choice() {
        assert_eq!(choose(&[], Some(0), true), None);
        assert_eq!(choose(&[], None, false), None);
    }

    #[test]
    fn strictly_lower_load_always_wins_over_affinity() {
        for rotate in [false, true] {
            assert_eq!(choose(&[(0, 3), (1, 1)], Some(0), rotate), Some(1));
        }
    }

    #[test]
    fn a_tie_keeps_the_last_cpu_unless_rotation_is_due() {
        assert_eq!(choose(&[(0, 1), (1, 1)], Some(0), false), Some(0));
        assert_eq!(choose(&[(0, 1), (1, 1)], Some(0), true), Some(1));
    }

    #[test]
    fn rotation_with_no_other_minimum_stays_put() {
        assert_eq!(choose(&[(0, 1), (1, 2)], Some(0), true), Some(0));
    }

    #[test]
    fn rotation_is_irrelevant_when_last_is_not_a_minimum() {
        assert_eq!(choose(&[(0, 2), (1, 1), (2, 1)], Some(0), true), Some(1));
        assert_eq!(choose(&[(0, 2), (1, 1), (2, 1)], Some(0), false), Some(1));
    }

    #[test]
    fn a_lower_load_found_later_discards_earlier_tied_candidates() {
        // 前两个与 last 并列于负载 2，但 CPU 2 的负载更低：last 已不是最小者。
        assert_eq!(choose(&[(1, 2), (0, 2), (2, 1)], Some(0), true), Some(2));
    }

    #[test]
    fn the_scan_order_breaks_ties_among_equal_candidates() {
        assert_eq!(choose(&[(2, 0), (1, 0)], None, false), Some(2));
        assert_eq!(choose(&[(0, 1), (2, 1), (1, 1)], Some(0), true), Some(2));
    }

    #[test]
    fn a_last_cpu_outside_the_candidates_falls_back_to_the_first_minimum() {
        assert_eq!(choose(&[(1, 1), (2, 1)], Some(0), true), Some(1));
    }

    #[test]
    fn a_recent_migrant_does_not_take_the_next_balancing_turn() {
        // 两核负载并列时，刚迁入的线程只运行了半个窗口，驻留更久的线程接过轮转机会。
        let after_a_leaves = [(0, 1), (1, 1)];
        let quantum_us = 10_000;
        let window = quantum_us * u64::from(ROTATION_INTERVAL_PREEMPTIONS);
        assert_eq!(
            choose(&after_a_leaves, Some(0), can_rotate(window / 2, quantum_us)),
            Some(0)
        );
        assert_eq!(
            choose(&after_a_leaves, Some(0), can_rotate(window, quantum_us)),
            Some(1)
        );
    }

    #[test]
    fn rotation_fires_once_per_interval_and_restarts() {
        let mut since = 0;
        let mut fired = alloc::vec::Vec::new();
        for preemption in 1..=3 * ROTATION_INTERVAL_PREEMPTIONS {
            let (rotate, next) = advance_rotation(since);
            since = next;
            if rotate {
                fired.push(preemption);
            }
        }
        let interval = ROTATION_INTERVAL_PREEMPTIONS;
        assert_eq!(fired, [interval, 2 * interval, 3 * interval]);
        assert_eq!(since, 0);
    }
}
