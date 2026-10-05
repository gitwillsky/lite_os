/// Final owner 对 prepared storage 的唯一处理结果。
pub(super) enum TimerTransactionCommit<Prepared, Output> {
    /// Publication succeeded and produced the syscall-visible output.
    Complete(Output),
    /// Final state changed; retry with an optional reusable unpublished owner.
    Retry(Option<Prepared>),
}

/// 执行 timer transaction 的唯一 prepare/recheck/commit retry protocol。
///
/// `plan` 完成后其 owner lock 已释放，`prepare` 才可分配；`final_commit` 必须自行完成
/// lifecycle recheck 与 owner publication。任一步失败时，当前 prepared value 由本函数栈帧
/// 唯一拥有并自动回收；只有 `Retry(Some(_))` 能把同一未发布 storage 带到下一轮。
pub(super) fn run_timer_transaction<Plan, Prepared, Output, Error>(
    mut initial_validate: impl FnMut() -> Result<(), Error>,
    mut plan: impl FnMut() -> Result<Plan, Error>,
    mut prepare: impl FnMut(Plan, Option<Prepared>) -> Result<Prepared, Error>,
    mut final_commit: impl FnMut(Prepared) -> Result<TimerTransactionCommit<Prepared, Output>, Error>,
) -> Result<Output, Error> {
    initial_validate()?;
    let mut reusable = None;
    loop {
        let plan = plan()?;
        let prepared = prepare(plan, reusable.take())?;
        match final_commit(prepared)? {
            TimerTransactionCommit::Complete(output) => return Ok(output),
            TimerTransactionCommit::Retry(prepared) => reusable = prepared,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct PreparedDrop(Arc<AtomicUsize>);

    impl Drop for PreparedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn lifecycle_change_during_prepare_prevents_publication_and_reclaims_owner() {
        let live = Arc::new(AtomicBool::new(true));
        let published = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(2));
        let changed = Arc::new(Barrier::new(2));
        let worker = {
            let live = live.clone();
            let start = start.clone();
            let changed = changed.clone();
            std::thread::spawn(move || {
                start.wait();
                live.store(false, Ordering::Release);
                changed.wait();
            })
        };

        let result = run_timer_transaction(
            || Ok::<_, &'static str>(()),
            || Ok(()),
            |(), _| {
                start.wait();
                changed.wait();
                Ok(PreparedDrop(drops.clone()))
            },
            |_prepared| {
                if !live.load(Ordering::Acquire) {
                    return Err("not-found");
                }
                published.store(true, Ordering::Relaxed);
                Ok(TimerTransactionCommit::Complete(()))
            },
        );
        worker.join().unwrap();
        assert_eq!(result, Err("not-found"));
        assert!(!published.load(Ordering::Relaxed));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn prepare_failure_never_enters_final_commit() {
        let commits = AtomicUsize::new(0);
        let result = run_timer_transaction(
            || Ok::<_, &'static str>(()),
            || Ok(()),
            |(), _: Option<()>| Err("oom"),
            |()| {
                commits.fetch_add(1, Ordering::Relaxed);
                Ok(TimerTransactionCommit::Complete(()))
            },
        );
        assert_eq!(result, Err("oom"));
        assert_eq!(commits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn collision_retry_reuses_the_same_unpublished_owner() {
        let plans = AtomicUsize::new(0);
        let allocations = AtomicUsize::new(0);
        let commits = AtomicUsize::new(0);
        let result = run_timer_transaction(
            || Ok::<_, ()>(()),
            || Ok(plans.fetch_add(1, Ordering::Relaxed)),
            |plan, reusable| {
                Ok(reusable.unwrap_or_else(|| {
                    allocations.fetch_add(1, Ordering::Relaxed);
                    plan
                }))
            },
            |prepared| {
                let attempt = commits.fetch_add(1, Ordering::Relaxed);
                Ok(if attempt == 0 {
                    TimerTransactionCommit::Retry(Some(prepared))
                } else {
                    TimerTransactionCommit::Complete(prepared)
                })
            },
        );
        assert_eq!(result, Ok(0));
        assert_eq!(plans.load(Ordering::Relaxed), 2);
        assert_eq!(allocations.load(Ordering::Relaxed), 1);
        assert_eq!(commits.load(Ordering::Relaxed), 2);
    }
}
