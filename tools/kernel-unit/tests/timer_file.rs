#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 timerfd backend/queue mutation、AVL deadline 与 Pipe；软中断摘取边界由 fixture 显式驱动。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

#[path = "../../../kernel/src/file"]
mod file {
    mod timerfd;
    pub(crate) use timerfd::{TimerError, TimerFdBackend, TimerFdRead, TimerSetting};
}

#[path = "../../../kernel/src/execution/task/process_table"]
mod task_table {
    use alloc::sync::Arc;
    struct ProcessTable {
        timers: spin::Mutex<timer_queue::TimerQueue>,
    }
    static PROCESS_TABLE: ProcessTable = ProcessTable {
        timers: spin::Mutex::new(timer_queue::TimerQueue::new()),
    };
    mod timer_queue {
        use crate::fallible_tree::{FallibleMap, VacantEntry};
        use crate::file::{TimerError, TimerFdBackend, TimerSetting};
        use alloc::sync::{Arc, Weak};
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum TimerIdentity {
            File(u64),
        }
        enum KernelTimer {}
        type DeadlineEntry = VacantEntry<(u64, TimerIdentity), Option<Weak<KernelTimer>>>;
        enum ExpiredTimer {
            File {
                target: Arc<dyn TimerFdBackend>,
                elapsed: u64,
                generation: u64,
                retired: TimerExpirationRetirement,
            },
            Silent(TimerExpirationRetirement),
        }
        #[derive(Default)]
        struct RetiredTimers {
            files: FallibleMap<u64, timer_file::TimerFile>,
            deadlines: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
        }
        struct TimerExpirationRetirement {
            _deadline: Option<DeadlineEntry>,
            file: Option<VacantEntry<u64, timer_file::TimerFile>>,
        }
        impl TimerExpirationRetirement {
            fn from_deadline(entry: Option<DeadlineEntry>) -> Self {
                Self {
                    _deadline: entry,
                    file: None,
                }
            }
        }
        pub(super) struct TimerQueue {
            timer_files: FallibleMap<u64, timer_file::TimerFile>,
            deadline_index: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
        }
        impl TimerQueue {
            pub(super) const fn new() -> Self {
                Self {
                    timer_files: FallibleMap::new(),
                    deadline_index: FallibleMap::new(),
                }
            }
        }
        mod period;
        use period::next_period;
        mod timer_file;
        pub(super) use timer_file::{TimerFileClock, create_timer_fd};
        pub(super) struct PendingExpiry(ExpiredTimer);
        impl PendingExpiry {
            pub(super) fn deliver(self) {
                match self.0 {
                    ExpiredTimer::File {
                        target,
                        elapsed,
                        generation,
                        retired,
                    } => {
                        drop(retired);
                        target.expire(elapsed, generation);
                    }
                    ExpiredTimer::Silent(_) => panic!("fixture lost live backend"),
                }
            }
        }
        pub(super) fn pop(now: u64) -> PendingExpiry {
            let mut queue = super::PROCESS_TABLE.timers.lock();
            let (expiration, TimerIdentity::File(id)) =
                *queue.deadline_index.first_key_value().unwrap().0;
            assert!(expiration <= now);
            let entry = queue
                .deadline_index
                .take_entry(&(expiration, TimerIdentity::File(id)))
                .unwrap();
            PendingExpiry(queue.pop_expired_timer_file(id, expiration, entry, now))
        }
        pub(super) fn next_deadline() -> Option<u64> {
            super::PROCESS_TABLE
                .timers
                .lock()
                .deadline_index
                .first_key_value()
                .map(|(key, _)| key.0)
        }
        pub(super) fn count() -> usize {
            super::PROCESS_TABLE.timers.lock().timer_files.len()
        }
    }
    use crate::file::{TimerError, TimerFdRead};
    use crate::namespace::{TimeOffset, TimeOffsets};
    use timer_queue::{count, create_timer_fd, next_deadline, pop};
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn stale_softirq_event_cannot_cross_rearm_disarm_or_expiration_reset() {
        let _serial = SERIAL.lock().unwrap();
        let timer = create_timer_fd(timer_queue::TimerFileClock::Monotonic).unwrap();
        let offsets = TimeOffsets::ZERO;
        for replacement in [0, 20] {
            timer.replace(10, 0, false, 0, offsets).unwrap();
            let old = pop(10);
            timer.replace(replacement, 0, false, 10, offsets).unwrap();
            old.deliver();
            assert_eq!(timer.read(), TimerFdRead::Empty);
            assert!(!timer.readable());
            if replacement != 0 {
                pop(30).deliver();
                assert_eq!(timer.read(), TimerFdRead::Expirations(1));
            }
        }
        timer.replace(10, 10, false, 0, offsets).unwrap();
        let old = pop(35);
        timer.replace(10, 10, false, 35, offsets).unwrap();
        old.deliver();
        assert_eq!(timer.read(), TimerFdRead::Empty);
        pop(65).deliver();
        assert_eq!(timer.read(), TimerFdRead::Expirations(3));
        drop(timer);
        assert_eq!(count(), 0);
    }
    #[test]
    fn actual_fput_retires_schedule_while_softirq_observer_keeps_backend_alive() {
        let _serial = SERIAL.lock().unwrap();
        let timer = create_timer_fd(timer_queue::TimerFileClock::Monotonic).unwrap();
        timer.replace(10, 10, false, 0, TimeOffsets::ZERO).unwrap();
        let stale = pop(10);
        let observer = timer.clone();
        timer.close();
        assert_eq!(count(), 0);
        assert_eq!(next_deadline(), None);
        assert_eq!(observer.setting(10), Err(TimerError::NotFound));
        stale.deliver();
        assert_eq!(observer.read(), TimerFdRead::Empty);
        observer.close();
        drop(timer);
        drop(observer);
        assert_eq!(count(), 0);
    }
    #[test]
    fn allocation_failure_preserves_old_counter_and_armed_node_is_reused() {
        let _serial = SERIAL.lock().unwrap();
        let timer = create_timer_fd(timer_queue::TimerFileClock::Monotonic).unwrap();
        timer.replace(10, 0, false, 0, TimeOffsets::ZERO).unwrap();
        pop(10).deliver();
        crate::allocation_failure::fail_next();
        assert_eq!(
            timer.replace(20, 0, false, 10, TimeOffsets::ZERO),
            Err(TimerError::OutOfMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(next_deadline(), None);
        assert_eq!(timer.read(), TimerFdRead::Expirations(1));
        timer.replace(10, 10, false, 0, TimeOffsets::ZERO).unwrap();
        crate::allocation_failure::fail_next();
        assert!(timer.replace(15, 10, false, 0, TimeOffsets::ZERO).is_ok());
        assert!(!crate::allocation_failure::was_consumed());
        assert_eq!(next_deadline(), Some(15));
        drop(timer);
        assert_eq!(count(), 0);
    }
    #[test]
    fn actual_clock_owner_selects_caller_offset_and_expired_nonzero_stays_armed() {
        let _serial = SERIAL.lock().unwrap();
        let offsets = TimeOffsets {
            monotonic: TimeOffset {
                seconds: 1,
                nanoseconds: 0,
            },
            boottime: TimeOffset {
                seconds: 2,
                nanoseconds: 0,
            },
        };
        for (clock, expected) in [
            (timer_queue::TimerFileClock::Monotonic, 2_000_000_000),
            (timer_queue::TimerFileClock::Boottime, 1_000_000_000),
            (timer_queue::TimerFileClock::Realtime, 2_999_999_000),
        ] {
            let timer = create_timer_fd(clock).unwrap();
            timer.replace(3_000_000_000, 0, true, 0, offsets).unwrap();
            assert_eq!(next_deadline(), Some(expected));
            timer
                .replace(3_000_000_000, 0, false, 100, offsets)
                .unwrap();
            assert_eq!(next_deadline(), Some(3_000_000_100));
            timer.replace(1, 0, true, 0, offsets).unwrap();
            assert_eq!(next_deadline(), Some(0));
            pop(0).deliver();
            assert_eq!(timer.read(), TimerFdRead::Expirations(1));
            timer.replace(0, 0, true, 0, offsets).unwrap();
            assert_eq!(next_deadline(), None);
            drop(timer);
        }
        assert_eq!(count(), 0);
    }
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
    pub(crate) mod timer {
        pub(crate) fn realtime_deadline_to_monotonic_ns(value: u64) -> u64 {
            value.saturating_sub(1_000)
        }
    }
}
