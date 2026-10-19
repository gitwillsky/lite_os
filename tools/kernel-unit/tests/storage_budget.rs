#![feature(allocator_ext)]
//! 执行生产配额 owner；只替换 IRQ-safe lock 的 host adapter。
#![allow(dead_code)]

extern crate alloc;

mod sync {
    pub(crate) struct IrqMutex<T>(spin::Mutex<T>);
    impl<T> IrqMutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(spin::Mutex::new(value))
        }
        pub(crate) fn lock(&self) -> spin::MutexGuard<'_, T> {
            self.0.lock()
        }
    }
}

#[path = "../../../kernel/src/fs"]
mod fs {
    #[derive(Debug, PartialEq, Eq)]
    enum FileSystemError {
        InvalidOperation,
        OutOfMemory,
    }
    mod storage_budget;
    use storage_budget::StorageBudget;

    #[test]
    #[ignore = "diagnostic CPU measurement; run alone in release"]
    fn production_storage_quota_cpu_cost() {
        let budget = StorageBudget::new(Some(1), Some(1)).unwrap();
        const ITERATIONS: usize = 100_000;
        let mut samples = [std::time::Duration::ZERO; 5];
        for sample in 0..=5 {
            let start = std::time::Instant::now();
            for _ in 0..ITERATIONS {
                assert!(std::hint::black_box(&budget).reserve_page());
                budget.release_page();
            }
            if sample != 0 {
                samples[sample - 1] = start.elapsed();
            }
        }
        samples.sort_unstable();
        println!(
            "storage quota reserve/release: {:.2} ns/op",
            samples[2].as_nanos() as f64 / ITERATIONS as f64
        );
    }

    #[test]
    fn unlimited_mount_cannot_gain_limits_on_remount() {
        let budget = StorageBudget::new(None, None).unwrap();
        assert_eq!(
            budget.set_limits(Some(10), None),
            Err(FileSystemError::InvalidOperation)
        );
        assert_eq!(
            budget.set_limits(None, Some(10)),
            Err(FileSystemError::InvalidOperation)
        );
        let state = budget.snapshot();
        assert_eq!((state.page_limit, state.inode_limit), (None, None));
    }

    #[test]
    fn remount_commits_both_quotas_or_keeps_both_old_limits() {
        let budget = StorageBudget::new(Some(4), Some(3)).unwrap();
        assert!(budget.reserve_page());
        assert!(budget.reserve_inode());
        assert_eq!(
            budget.set_limits(Some(10), Some(0)),
            Err(FileSystemError::InvalidOperation)
        );
        let state = budget.snapshot();
        assert_eq!(
            (
                state.page_limit,
                state.inode_limit,
                state.used_pages,
                state.used_inodes
            ),
            (Some(4), Some(3), 1, 1)
        );
        assert_eq!(
            budget.set_limits(Some(0), Some(10)),
            Err(FileSystemError::InvalidOperation)
        );
        budget.set_limits(Some(1), Some(1)).unwrap();
        assert!(!budget.reserve_page());
        assert!(!budget.reserve_inode());
        budget.release_page();
        budget.release_inode();
        budget.set_limits(Some(0), Some(0)).unwrap();
        assert!(!budget.reserve_page());
        assert!(!budget.reserve_inode());
        budget.set_limits(None, None).unwrap();
        assert!(budget.reserve_page());
        assert!(budget.reserve_inode());
        budget.release_page();
        budget.release_inode();
    }

    #[test]
    fn concurrent_remount_and_reservations_never_exceed_published_limits() {
        let budget = StorageBudget::new(Some(8), Some(8)).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(4));
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let budget = budget.clone();
                let start = start.clone();
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..20_000 {
                        let page = budget.reserve_page();
                        let inode = budget.reserve_inode();
                        let state = budget.snapshot();
                        assert!(state.used_pages <= state.page_limit.unwrap());
                        assert!(state.used_inodes <= state.inode_limit.unwrap());
                        if inode {
                            budget.release_inode();
                        }
                        if page {
                            budget.release_page();
                        }
                    }
                });
            }
            start.wait();
            for iteration in 0..20_000 {
                let limit = iteration % 9;
                let _ = budget.set_limits(Some(limit), Some(8 - limit));
                let state = budget.snapshot();
                assert!(state.used_pages <= state.page_limit.unwrap());
                assert!(state.used_inodes <= state.inode_limit.unwrap());
            }
        });
        let state = budget.snapshot();
        assert_eq!((state.used_pages, state.used_inodes), (0, 0));
        budget.set_limits(Some(0), Some(0)).unwrap();
    }
}
