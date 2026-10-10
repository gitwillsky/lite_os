//! 直接执行生产后台策略；仅替换 allocator snapshot 和 resident adapter 边界。
#![allow(dead_code)]

use std::cell::RefCell;

#[derive(Default)]
struct Fixture {
    capacity: usize,
    free: usize,
    reclaimable: usize,
    requests: Vec<usize>,
    delays: Vec<u64>,
    free_after_poll: Vec<Option<usize>>,
    reclaimable_after_poll: Vec<Option<usize>>,
    polls: usize,
}

thread_local! {
    static FIXTURE: RefCell<Fixture> = RefCell::new(Fixture::default());
}

#[path = "../../../kernel/src/memory"]
mod memory {
    const KERNEL_HEAP_GROWTH_PAGES: usize = 64;

    mod frame_allocator {
        pub(super) struct FrameStatistics {
            pub(super) capacity_pages: usize,
            pub(super) free_pages: usize,
        }
        pub(super) fn statistics() -> FrameStatistics {
            crate::FIXTURE.with(|fixture| {
                let fixture = fixture.borrow();
                FrameStatistics {
                    capacity_pages: fixture.capacity,
                    free_pages: fixture.free,
                }
            })
        }
    }

    mod shared_file {
        pub(super) struct ReclaimResult(usize);
        impl ReclaimResult {
            pub(super) fn reclaimed_pages(self) -> usize {
                self.0
            }
        }
        pub(super) fn reclaim_background_pages(limit: usize) -> ReclaimResult {
            crate::FIXTURE.with(|fixture| {
                let mut fixture = fixture.borrow_mut();
                fixture.requests.push(limit);
                let released = limit.min(fixture.reclaimable);
                fixture.free += released;
                fixture.reclaimable -= released;
                ReclaimResult(released)
            })
        }
    }

    pub(crate) mod pressure;
}

fn run_polls() {
    let stopped = std::panic::catch_unwind(|| {
        memory::pressure::run_background_reclaim(
            || 0,
            |deadline| {
                let finished = FIXTURE.with(|fixture| {
                    let mut fixture = fixture.borrow_mut();
                    fixture.delays.push(deadline);
                    let index = fixture.delays.len() - 1;
                    if let Some(Some(free)) = fixture.free_after_poll.get(index) {
                        fixture.free = *free;
                    }
                    if let Some(Some(pages)) = fixture.reclaimable_after_poll.get(index) {
                        fixture.reclaimable = *pages;
                    }
                    fixture.delays.len() == fixture.polls
                });
                if finished {
                    std::panic::panic_any(StopAfterPolls);
                }
            },
        )
    });
    assert!(stopped.unwrap_err().is::<StopAfterPolls>());
}

#[derive(Debug)]
struct StopAfterPolls;

#[test]
fn low_to_high_hysteresis_and_bounded_batches() {
    FIXTURE.with(|fixture| {
        *fixture.borrow_mut() = Fixture {
            capacity: 32768,
            free: 512,
            reclaimable: 4096,
            free_after_poll: [Some(511)]
                .into_iter()
                .chain([None; 9])
                .chain([Some(800)])
                .collect(),
            polls: 12,
            ..Default::default()
        };
    });
    run_polls();
    FIXTURE.with(|fixture| {
        let fixture = fixture.borrow();
        // 首批后 free 已回到低水位上方，仍继续到 high；最后一批精确补齐。
        assert_eq!(fixture.requests, [64, 64, 64, 64, 64, 64, 64, 64, 1]);
        assert_eq!(fixture.delays[0], 100_000_000);
        assert_eq!(&fixture.delays[1..10], &[10_000_000; 9]);
        // 高水位退出后，中间区间不会自动重启。
        assert_eq!(&fixture.delays[10..], &[100_000_000; 2]);
        assert_eq!(fixture.free, 800);
    });
}

#[test]
fn no_progress_backs_off_and_recovers_without_busy_loop() {
    FIXTURE.with(|fixture| {
        *fixture.borrow_mut() = Fixture {
            capacity: usize::MAX,
            free: 100,
            reclaimable_after_poll: vec![None, None, None, Some(1)],
            free_after_poll: vec![None, None, None, None, None, Some(usize::MAX)],
            polls: 7,
            ..Default::default()
        };
    });
    run_polls();
    FIXTURE.with(|fixture| {
        let fixture = fixture.borrow();
        assert_eq!(fixture.requests, [64; 6]);
        assert_eq!(
            fixture.delays,
            [
                100_000_000,
                100_000_000,
                100_000_000,
                100_000_000,
                10_000_000,
                100_000_000,
                100_000_000
            ]
        );
    });
}
