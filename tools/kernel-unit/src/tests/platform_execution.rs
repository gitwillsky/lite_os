use crate::{plic_policy, sv39, timer_deadline};

#[cfg(test)]
mod timer_deadline_tests {
    use super::timer_deadline::next;

    #[test]
    fn first_deadline_starts_one_interval_after_now() {
        assert_eq!(next(0, 100, 25), Some(125));
    }

    #[test]
    fn delayed_handler_preserves_phase_and_skips_missed_ticks() {
        assert_eq!(next(100, 100, 25), Some(125));
        assert_eq!(next(100, 149, 25), Some(150));
        assert_eq!(next(100, 150, 25), Some(175));
    }

    #[test]
    fn future_deadline_is_not_reprogrammed() {
        assert_eq!(next(200, 150, 25), Some(200));
    }

    #[test]
    fn invalid_or_exhausted_deadline_is_rejected() {
        assert_eq!(next(100, 100, 0), None);
        assert_eq!(next(0, u64::MAX, 1), None);
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
