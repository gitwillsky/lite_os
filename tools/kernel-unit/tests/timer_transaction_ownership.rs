#![feature(allocator_ext)]
#![allow(dead_code)]
//! 执行实际 borrowed timer executor 和 AVL storage；graph liveness 与 clock 是显式输入 seam。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/execution/task/process_table"]
mod task_table {
    use crate::fallible_tree::FallibleMap;
    struct FixtureThread {
        identity: alloc::sync::Arc<crate::namespace::PidIdentity>,
    }
    impl FixtureThread {
        fn tid_identity(&self) -> alloc::sync::Arc<crate::namespace::PidIdentity> {
            self.identity.clone()
        }
    }
    struct ProcessGraph {
        live: bool,
        threads: FallibleMap<usize, FixtureThread>,
    }
    struct ProcessTable {
        graph: spin::Mutex<ProcessGraph>,
        timers: spin::Mutex<timer_queue::TimerQueue>,
    }
    static PROCESS_TABLE: ProcessTable = ProcessTable {
        graph: spin::Mutex::new(ProcessGraph {
            live: true,
            threads: FallibleMap::new(),
        }),
        timers: spin::Mutex::new(timer_queue::TimerQueue { committed: false }),
    };
    mod timer_queue {
        use crate::fallible_tree::{FallibleMap, VacantEntry};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum TimerError {
            NotFound,
            OutOfMemory,
        }
        #[derive(Clone, Copy)]
        enum PosixTimerNotification {
            None,
            Thread { tid: usize },
        }
        pub(super) struct TimerQueue {
            pub(super) committed: bool,
        }
        fn live_process(
            graph: &super::ProcessGraph,
            _: usize,
        ) -> Result<&FallibleMap<usize, super::FixtureThread>, TimerError> {
            if graph.live {
                Ok(&graph.threads)
            } else {
                Err(TimerError::NotFound)
            }
        }
        mod transaction;
        mod transaction_loop;
        use transaction::{TimerTransactionPolicy, execute_timer_transaction};
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        struct Trace {
            drops: AtomicUsize,
            dropped_under_owner: AtomicBool,
        }
        struct Probe(Arc<Trace>);
        impl Drop for Probe {
            fn drop(&mut self) {
                let bad = super::PROCESS_TABLE.graph.try_lock().is_none()
                    || super::PROCESS_TABLE.timers.try_lock().is_none();
                self.0.dropped_under_owner.fetch_or(bad, Ordering::Relaxed);
                self.0.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        struct Prepared {
            node: Option<VacantEntry<usize, Probe>>,
        }
        fn trace() -> Arc<Trace> {
            Arc::new(Trace {
                drops: AtomicUsize::new(0),
                dropped_under_owner: AtomicBool::new(false),
            })
        }
        fn node(trace: &Arc<Trace>) -> Prepared {
            assert!(super::PROCESS_TABLE.graph.try_lock().is_some());
            assert!(super::PROCESS_TABLE.timers.try_lock().is_some());
            Prepared {
                node: Some(FallibleMap::try_prepare(1, Probe(trace.clone())).unwrap()),
            }
        }
        fn reset() {
            super::PROCESS_TABLE.graph.lock().live = true;
            super::PROCESS_TABLE.timers.lock().committed = false;
        }
        #[test]
        fn unused_reserved_node_is_destroyed_after_success_releases_both_guards() {
            let _serial = SERIAL.lock().unwrap();
            reset();
            let trace = trace();
            let result = execute_timer_transaction(
                TimerTransactionPolicy::itimer_real(1),
                |_| Ok(()),
                |(), _: Option<Prepared>| Ok(node(&trace)),
                |timers, prepared| {
                    assert!(super::PROCESS_TABLE.graph.try_lock().is_none());
                    assert!(super::PROCESS_TABLE.timers.try_lock().is_none());
                    assert!(prepared.node.is_some());
                    // 实际已有 node 时留给 executor 锁外回收预备 storage。
                    timers.committed = true;
                    Ok(Some(7))
                },
            );
            assert_eq!(result, Ok(7));
            assert_eq!(trace.drops.load(Ordering::Relaxed), 1);
            assert!(!trace.dropped_under_owner.load(Ordering::Relaxed));
        }
        #[test]
        fn stale_plan_reuses_same_actual_node_and_failed_final_liveness_retires_outside_guards() {
            let _serial = SERIAL.lock().unwrap();
            reset();
            let trace = trace();
            let plans = AtomicUsize::new(0);
            let mut identity = None;
            let result = execute_timer_transaction(
                TimerTransactionPolicy::itimer_real(1),
                |_| Ok(plans.fetch_add(1, Ordering::Relaxed)),
                |round, reusable| {
                    if round == 0 {
                        let prepared = node(&trace);
                        identity = Some(
                            core::ptr::from_ref(prepared.node.as_ref().unwrap().value()).addr(),
                        );
                        Ok(prepared)
                    } else {
                        let prepared: Prepared = reusable.expect("retry must keep actual node");
                        assert_eq!(
                            identity,
                            Some(
                                core::ptr::from_ref(prepared.node.as_ref().unwrap().value()).addr()
                            )
                        );
                        assert_eq!(trace.drops.load(Ordering::Relaxed), 0);
                        super::PROCESS_TABLE.graph.lock().live = false;
                        Ok(prepared)
                    }
                },
                |_, _| Ok(None::<()>),
            );
            assert_eq!(result, Err(TimerError::NotFound));
            assert_eq!(plans.load(Ordering::Relaxed), 2);
            assert_eq!(trace.drops.load(Ordering::Relaxed), 1);
            assert!(!trace.dropped_under_owner.load(Ordering::Relaxed));
            reset();
        }
        #[test]
        fn actual_removed_node_output_is_owned_until_caller_retires_after_commit() {
            let _serial = SERIAL.lock().unwrap();
            reset();
            let trace = trace();
            let mut retired = execute_timer_transaction(
                TimerTransactionPolicy::itimer_real(1),
                |_| Ok(()),
                |(), _: Option<Prepared>| Ok(node(&trace)),
                |_, prepared| Ok(Some(prepared.node.take().unwrap())),
            )
            .unwrap();
            assert_eq!(trace.drops.load(Ordering::Relaxed), 0);
            retired.set_key(2);
            drop(retired);
            assert_eq!(trace.drops.load(Ordering::Relaxed), 1);
            assert!(!trace.dropped_under_owner.load(Ordering::Relaxed));
        }
    }
}
