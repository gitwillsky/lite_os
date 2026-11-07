use crate::{plic_policy, sv39, timer_deadline};

#[cfg(test)]
mod timer_deadline_tests {
    use super::timer_deadline::DeadlineIndex;

    fn entry(deadline: u64, ticket: u64) -> fallible_tree::VacantEntry<(u64, u64), u32> {
        DeadlineIndex::prepare((deadline, ticket), ticket as u32).unwrap()
    }

    #[test]
    fn only_a_new_earliest_deadline_changes_the_head() {
        let mut index = DeadlineIndex::new();
        assert!(index.insert(entry(100, 1)), "first entry is the head");
        assert!(!index.insert(entry(200, 2)), "later deadline keeps the head");
        assert!(index.insert(entry(50, 3)), "earlier deadline becomes the head");
        assert_eq!(index.head(), Some(50));
    }

    #[test]
    fn equal_deadlines_are_served_first_in_first_out() {
        let mut index = DeadlineIndex::new();
        for ticket in [3, 1, 2] {
            index.insert(entry(100, ticket));
        }
        let mut batch = [const { None }; 8];
        let due = index.take_due(100, &mut batch);
        assert_eq!(due.taken, 3);
        let order: Vec<_> = batch.iter().flatten().map(|e| e.key().1).collect();
        assert_eq!(order, [1, 2, 3]);
    }

    #[test]
    fn cancelling_reports_a_head_change_only_for_the_head() {
        let mut index = DeadlineIndex::new();
        index.insert(entry(100, 1));
        index.insert(entry(200, 2));
        let tail = index.cancel(&(200, 2));
        assert!(tail.removed.is_some() && !tail.head_changed);
        let gone = index.cancel(&(200, 2));
        assert!(gone.removed.is_none() && !gone.head_changed, "already expired");
        let head = index.cancel(&(100, 1));
        assert!(head.removed.is_some() && head.head_changed);
        assert_eq!(index.head(), None);
    }

    #[test]
    fn expiry_is_batched_and_reports_remaining_due_entries() {
        let mut index = DeadlineIndex::new();
        for ticket in 0..5 {
            index.insert(entry(10 + ticket, ticket));
        }
        index.insert(entry(1000, 9));
        let mut batch = [const { None }; 3];
        let first = index.take_due(100, &mut batch);
        assert_eq!((first.taken, first.more_due), (3, true));
        let second = index.take_due(100, &mut batch);
        assert_eq!((second.taken, second.more_due), (2, false));
        assert_eq!(index.head(), Some(1000));
        let none = index.take_due(100, &mut batch);
        assert_eq!((none.taken, none.more_due), (0, false));
    }
}

#[cfg(test)]
mod futex_queue_tests {
    use super::super::futex_queue::{FutexKey, Requeue, RequeueHooks, Table, requeue_locked, wake_locked};

    fn private(address: usize) -> FutexKey {
        FutexKey::Private { space: 1, address }
    }

    fn enqueue(table: &mut Table<u32>, key: FutexKey, ticket: u64, waiter: u32) {
        let entry = fallible_tree::FallibleMap::try_prepare((key, ticket), waiter).unwrap();
        table.commit_vacant(entry);
    }

    #[test]
    fn wake_is_fifo_per_key_and_never_touches_other_keys() {
        let mut table = Table::new();
        let (a, b) = (private(0x1000), private(0x2000));
        for (ticket, key, waiter) in [(5, a, 50), (1, a, 10), (3, b, 30), (2, a, 20)] {
            enqueue(&mut table, key, ticket, waiter);
        }
        let mut woken = Vec::new();
        assert_eq!(wake_locked(&mut table, a, 2, |w| woken.push(w)), 2);
        assert_eq!(woken, [10, 20], "oldest registrations first");
        assert_eq!(wake_locked(&mut table, a, usize::MAX, |w| woken.push(w)), 1);
        assert_eq!(woken, [10, 20, 50]);
        assert_eq!(wake_locked(&mut table, a, 1, |_| panic!("no waiter left")), 0);
        assert_eq!(table.len(), 1, "the other key is untouched");
    }

    struct Hooks {
        woken: Vec<u32>,
        moved: Vec<(u32, (FutexKey, u64))>,
        ticket: u64,
    }

    impl RequeueHooks<u32> for Hooks {
        fn wake(&mut self, waiter: u32) {
            self.woken.push(waiter);
        }
        fn next_ticket(&mut self) -> u64 {
            self.ticket += 1;
            self.ticket
        }
        fn moved(&mut self, waiter: &u32, position: (FutexKey, u64)) {
            self.moved.push((*waiter, position));
        }
    }

    fn hooks() -> Hooks {
        Hooks {
            woken: Vec::new(),
            moved: Vec::new(),
            ticket: 100,
        }
    }

    #[test]
    fn requeue_wakes_first_then_moves_behind_the_existing_target_waiters() {
        let (source, target) = (private(0x1000), private(0x2000));
        let mut from = Table::new();
        let mut to = Table::new();
        for ticket in 1..=4u64 {
            enqueue(&mut from, source, ticket, ticket as u32);
        }
        enqueue(&mut to, target, 7, 70);
        let mut hooks = hooks();
        let plan = Requeue { source, target, wake: 1, requeue: 2 };
        assert_eq!(requeue_locked(&mut from, Some(&mut to), &plan, &mut hooks), 2);
        assert_eq!(hooks.woken, [1]);
        assert_eq!(hooks.moved.iter().map(|m| m.0).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(from.len(), 1, "the fourth waiter stays on the source");
        // 移动后的等待者排在目标原有等待者之后，且保持彼此的先后。
        let mut order = Vec::new();
        wake_locked(&mut to, target, usize::MAX, |w| order.push(w));
        assert_eq!(order, [70, 2, 3]);
    }

    #[test]
    fn requeue_within_one_bucket_keeps_a_single_table_consistent() {
        let (source, target) = (private(0x1000), private(0x1004));
        let mut table = Table::new();
        for ticket in 1..=3u64 {
            enqueue(&mut table, source, ticket, ticket as u32);
        }
        let mut hooks = hooks();
        let plan = Requeue { source, target, wake: 0, requeue: usize::MAX };
        assert_eq!(requeue_locked(&mut table, None, &plan, &mut hooks), 3);
        assert_eq!(wake_locked(&mut table, source, 1, |_| panic!("source drained")), 0);
        let mut order = Vec::new();
        wake_locked(&mut table, target, usize::MAX, |w| order.push(w));
        assert_eq!(order, [1, 2, 3]);
    }

    #[test]
    fn a_key_always_maps_to_the_same_bucket_and_buckets_spread() {
        let buckets = 1024;
        let key = FutexKey::Shared { vmo: 9, offset: 0x40 };
        assert_eq!(key.bucket(buckets), key.bucket(buckets));
        let mut seen = std::collections::HashSet::new();
        for address in (0..4096usize).map(|i| 0x10_0000 + i * 4) {
            let index = private(address).bucket(buckets);
            assert!(index < buckets);
            seen.insert(index);
        }
        assert!(seen.len() > buckets * 9 / 10, "adjacent futex words must spread");
    }
}

#[cfg(test)]
mod plic_policy_tests {
    use alloc::{collections::VecDeque, vec::Vec};

    use super::plic_policy::{
        HARDIRQ_CLAIM_BUDGET, dispatch_claim_batch, enable_word_offset, valid_interrupt_vector,
    };

    #[test]
    fn plic_vector_geometry_rejects_reserved_and_cross_context_boundaries() {
        assert!(!valid_interrupt_vector(0));
        assert_eq!(enable_word_offset(0), None);

        assert!(valid_interrupt_vector(1023));
        assert_eq!(enable_word_offset(1023), Some(0x7c));

        assert!(!valid_interrupt_vector(1024));
        assert_eq!(enable_word_offset(1024), None);
    }

    #[derive(Debug, PartialEq, Eq)]
    enum HandlerError {
        Failed,
    }

    #[test]
    fn handler_error_still_completes_each_claim_exactly_once() {
        let mut pending = [7, 8, 0].into_iter();
        let mut handled = Vec::new();
        let mut completed = Vec::new();

        let result = dispatch_claim_batch(
            || pending.next().unwrap(),
            |vector| {
                handled.push(vector);
                if vector == 7 {
                    Err(HandlerError::Failed)
                } else {
                    Ok(())
                }
            },
            |vector| completed.push(vector),
        );

        assert_eq!(result, Err(HandlerError::Failed));
        assert_eq!(handled, [7, 8]);
        assert_eq!(completed, [7, 8]);
    }

    #[test]
    fn exhausted_budget_leaves_next_claim_pending_for_retrigger() {
        let end = u32::try_from(HARDIRQ_CLAIM_BUDGET).unwrap() + 2;
        let mut pending: VecDeque<_> = (1..=end).chain([0]).collect();
        let mut completed = Vec::new();

        dispatch_claim_batch(
            || pending.pop_front().unwrap(),
            |_| Ok::<(), ()>(()),
            |vector| completed.push(vector),
        )
        .unwrap();

        assert_eq!(completed.len(), HARDIRQ_CLAIM_BUDGET);
        assert_eq!(pending.front().copied(), Some(end - 1));

        dispatch_claim_batch(
            || pending.pop_front().unwrap(),
            |_| Ok::<(), ()>(()),
            |vector| completed.push(vector),
        )
        .unwrap();

        assert_eq!(completed.len(), HARDIRQ_CLAIM_BUDGET + 2);
        assert_eq!(pending.front().copied(), None);
    }
}

#[cfg(test)]
mod sv39_tests {
    use super::sv39::indexes;

    #[test]
    fn virtual_page_number_splits_into_three_nine_bit_indexes() {
        assert_eq!(indexes(0), [0, 0, 0]);
        assert_eq!(indexes(0x7fff_ffff), [0x1ff, 0x1ff, 0x1ff]);
        assert_eq!(indexes((3 << 18) | (7 << 9) | 11), [3, 7, 11]);
    }
}
