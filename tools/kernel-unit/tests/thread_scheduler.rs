#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production scheduling owner/policy/proc projection. IRQ, topology and TCB access are host seams.
extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; }
struct FaultAllocator;
// SAFETY: injection only changes failure return; successful allocations use System unchanged.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NEXT
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            core::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;

mod devices {
    pub(crate) mod model {

        pub(crate) mod io_completion {
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(crate) struct IoWaitKey;
        }
    }
}

#[path = "."]
pub(crate) mod execution {
    pub(crate) mod cpu {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct CpuId(pub(crate) usize);
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct CpuSet(usize);
        impl CpuSet {
            pub(crate) fn from_native_word(value: usize) -> Self {
                Self(value)
            }
            pub(crate) fn native_word(self) -> usize {
                self.0
            }
            pub(crate) fn is_empty(self) -> bool {
                self.0 == 0
            }
            pub(crate) fn contains(self, cpu: CpuId) -> bool {
                self.0 & (1 << cpu.0) != 0
            }
        }
        impl core::ops::BitAnd for CpuSet {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }
        pub(crate) fn active() -> CpuSet {
            CpuSet(3)
        }
        pub(crate) fn possible() -> CpuSet {
            active()
        }
    }
    pub(crate) mod sync {
        pub(crate) type IrqMutex<T> = spin::Mutex<T>;
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct TaskWaitKey;
        impl TaskWaitKey {
            // 显式 scheduler adapter 输入事实；native mode 编码由实际 sync/task_wait 的用例覆盖。
            pub(crate) fn idle(self) -> bool {
                false
            }
            pub(crate) fn contributes_to_load(self) -> bool {
                false
            }
        }
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum WaitResult {
            Woken,
        }
    }
    #[path = "../../../kernel/src/execution/task"]
    pub(crate) mod task {
        pub(crate) mod processor {
            pub(crate) fn account_current_cpu_runtime(_: u64) {}
        }
        #[path = "model"]
        pub(crate) mod model {
            use crate::{execution::cpu::CpuId, execution::sync::IrqMutex};
            use alloc::sync::Arc;
            use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
            use spin::Mutex;
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            enum StopResume {
                New,
                Runnable,
                Blocked,
            }
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            enum StopTransition {
                Running,
            }
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            enum RunState {
                New,
                Ready {
                    cpu: CpuId,
                    generation: u64,
                },
                Running {
                    cpu: CpuId,
                },
                Preempting {
                    cpu: CpuId,
                },
                Blocking {
                    cpu: CpuId,
                },
                Blocked,
                WakePending {
                    cpu: CpuId,
                },
                StopPending {
                    cpu: CpuId,
                    transition: StopTransition,
                },
                Stopped {
                    resume: StopResume,
                },
                Exited,
            }
            mod scheduling;
            use scheduling::{CpuAffinity, Sched, SchedulingEntity};
            struct Process {
                tgid: usize,
                start_time_us: u64,
                leader_scheduler: Mutex<Arc<SchedulingEntity>>,
                cpu_runtime_us: Arc<AtomicU64>,
            }
            struct Execution {
                tid: AtomicUsize,
                start_time_us: u64,
            }
            struct TaskControlBlock {
                process: Arc<Process>,
                execution: Execution,
                scheduling: Arc<SchedulingEntity>,
                kernel: bool,
            }
            impl TaskControlBlock {
                fn process(&self) -> &Process {
                    &self.process
                }
                fn is_kernel_thread(&self) -> bool {
                    self.kernel
                }
                fn tid(&self) -> usize {
                    self.execution.tid.load(Ordering::Acquire)
                }
                fn tgid(&self) -> usize {
                    self.process.tgid
                }
            }
            fn owner(nice: i32, cpu: usize, counter: Arc<AtomicU64>) -> Arc<SchedulingEntity> {
                SchedulingEntity::try_new(
                    Sched::new(nice, 0, counter),
                    CpuAffinity::all_possible(),
                    cpu,
                )
                .unwrap()
            }
            fn tasks() -> (TaskControlBlock, TaskControlBlock) {
                let counter = Arc::new(AtomicU64::new(0));
                let leader = owner(0, 0, counter.clone());
                let process = Arc::new(Process {
                    tgid: 10,
                    start_time_us: 100,
                    leader_scheduler: Mutex::new(leader.clone()),
                    cpu_runtime_us: counter.clone(),
                });
                let leader = TaskControlBlock {
                    process: process.clone(),
                    execution: Execution {
                        tid: AtomicUsize::new(10),
                        start_time_us: 100,
                    },
                    scheduling: leader,
                    kernel: false,
                };
                let worker = TaskControlBlock {
                    process,
                    execution: Execution {
                        tid: AtomicUsize::new(11),
                        start_time_us: 500,
                    },
                    scheduling: owner(5, 1, counter),
                    kernel: false,
                };
                (leader, worker)
            }
            #[test]
            fn blocked_load_follows_actual_deschedule_wake_stop_and_exit_without_allocations() {
                use scheduling::{SchedulingState, WaitMembership, uninterruptible_task_count};
                let before = uninterruptible_task_count();
                let mut state = SchedulingState::new(CpuAffinity::all_possible());
                state.replace_non_ready_state(RunState::Running { cpu: CpuId(0) });
                state.wait = Some(WaitMembership::DriverIo(
                    crate::devices::model::io_completion::IoWaitKey,
                ));
                state.replace_non_ready_state(RunState::Blocking { cpu: CpuId(0) });
                assert!(state.is_uninterruptible());
                assert_eq!(
                    uninterruptible_task_count(),
                    before,
                    "current still owns running admission"
                );
                crate::FAIL_NEXT.with(|flag| flag.set(true));
                state.replace_non_ready_state(RunState::Blocked);
                assert_eq!(uninterruptible_task_count(), before + 1);
                state.replace_non_ready_state(RunState::Blocked);
                assert_eq!(
                    uninterruptible_task_count(),
                    before + 1,
                    "no duplicate admission"
                );
                state.replace_non_ready_state(RunState::Stopped {
                    resume: StopResume::Blocked,
                });
                assert!(!state.is_uninterruptible());
                assert_eq!(uninterruptible_task_count(), before);
                state.replace_non_ready_state(RunState::Blocked);
                state.wait = None;
                let ready = state.transition_to_ready(CpuId(1));
                ready.consume_ready_projection_parts();
                assert_eq!(
                    uninterruptible_task_count(),
                    before,
                    "cross-CPU wake retires original blocked admission"
                );
                assert!(
                    crate::FAIL_NEXT.with(|flag| flag.replace(false)),
                    "state transitions must not allocate"
                );
                let mut early = SchedulingState::new(CpuAffinity::all_possible());
                early.wait = Some(WaitMembership::Vfork(23));
                early.replace_non_ready_state(RunState::Blocking { cpu: CpuId(0) });
                early.wait = None;
                early.replace_non_ready_state(RunState::WakePending { cpu: CpuId(0) });
                assert_eq!(
                    uninterruptible_task_count(),
                    before,
                    "early wake never creates blocked admission"
                );
                let mut exiting = SchedulingState::new(CpuAffinity::all_possible());
                exiting.wait = Some(WaitMembership::Vfork(24));
                exiting.replace_non_ready_state(RunState::Blocked);
                assert_eq!(uninterruptible_task_count(), before + 1);
                exiting.wait = None;
                exiting.replace_non_ready_state(RunState::Exited);
                assert_eq!(uninterruptible_task_count(), before);
                let mut interruptible = SchedulingState::new(CpuAffinity::all_possible());
                interruptible.wait = Some(WaitMembership::Pipe(12));
                interruptible.replace_non_ready_state(RunState::Blocked);
                assert!(!interruptible.is_uninterruptible());
                assert_eq!(uninterruptible_task_count(), before);
            }
            #[test]
            fn retired_leader_scheduler_is_same_owner_without_retaining_tcb() {
                let (leader, worker) = tasks();
                let leader = Arc::new(leader);
                let weak_task = Arc::downgrade(&leader);
                let weak_scheduler = Arc::downgrade(&leader.scheduling);
                leader.scheduling.policy.lock().nice(Some(2));
                drop(leader);
                assert!(weak_task.upgrade().is_none());
                assert_eq!(worker.process().process_scheduler_snapshot(), (2, 22, 0, 0));
                assert_eq!(worker.thread_statistics(None), (500, 5, 25, 0));
                drop(worker);
                assert!(weak_scheduler.upgrade().is_none());
            }
            #[test]
            fn promotion_uses_original_birthdate_and_retarget_uses_calling_policy() {
                let (leader, worker) = tasks();
                drop(leader);
                let old = core::mem::replace(
                    &mut *worker.process.leader_scheduler.lock(),
                    worker.scheduling.clone(),
                );
                drop(old);
                worker.execution.tid.store(worker.tgid(), Ordering::Release);
                assert_eq!(worker.process().process_scheduler_snapshot(), (5, 25, 1, 0));
                assert_eq!(worker.thread_statistics(None), (100, 5, 25, 0));
            }
            #[test]
            fn fallible_owner_preserves_counter_and_alias_ready_state_is_canonical() {
                let counter = Arc::new(AtomicU64::new(0));
                let policy = Sched::new(0, 0, counter.clone());
                crate::FAIL_NEXT.set(true);
                assert!(SchedulingEntity::try_new(policy, CpuAffinity::all_possible(), 0).is_err());
                assert!(!crate::FAIL_NEXT.get());
                assert_eq!(Arc::strong_count(&counter), 1);
                let scheduler = owner(0, 0, counter.clone());
                let alias = scheduler.clone();
                let generation = scheduler
                    .state
                    .lock()
                    .transition_to_ready(CpuId(0))
                    .consume_ready_projection_parts()
                    .2;
                assert_eq!(
                    alias.state.lock().run_state(),
                    RunState::Ready {
                        cpu: CpuId(0),
                        generation
                    }
                );
                alias
                    .state
                    .lock()
                    .transition_ready_to_running(CpuId(0), generation)
                    .consume_ready_projection_cpu();
                scheduler.policy.lock().begin_runtime(100);
                alias.policy.lock().checkpoint_runtime(110);
                scheduler.policy.lock().finish_runtime(120);
                assert_eq!(counter.load(Ordering::Relaxed), 20);
                assert_eq!(
                    alias.state.lock().run_state(),
                    RunState::Running { cpu: CpuId(0) }
                );
            }
            #[test]
            #[ignore = "diagnostic CPU measurement; run alone in release"]
            fn scheduling_arc_cpu_cost() {
                use std::{hint::black_box, time::Instant};
                struct OldInline {
                    scheduling: SchedulingEntity,
                }
                let counter = Arc::new(AtomicU64::new(0));
                let inline = OldInline {
                    scheduling: Arc::try_unwrap(owner(0, 0, counter.clone())).ok().unwrap(),
                };
                let (_, task) = tasks();
                let task = Arc::new(task);
                fn measure(mut operation: impl FnMut()) -> f64 {
                    for _ in 0..10000 {
                        operation();
                    }
                    let mut samples = [0f64; 5];
                    for sample in &mut samples {
                        let start = Instant::now();
                        for _ in 0..100000 {
                            operation();
                        }
                        *sample = start.elapsed().as_nanos() as f64 / 100000.;
                    }
                    samples.sort_by(f64::total_cmp);
                    samples[2]
                }
                let inline = Arc::new(inline);
                let old = measure(|| {
                    black_box(
                        black_box(&inline)
                            .scheduling
                            .policy
                            .lock()
                            .get_dynamic_priority(),
                    );
                });
                let new = measure(|| {
                    black_box(
                        black_box(&task)
                            .scheduling
                            .policy
                            .lock()
                            .get_dynamic_priority(),
                    );
                });
                let proc = measure(|| {
                    black_box(black_box(&task).process().process_scheduler_snapshot());
                });
                println!(
                    "actual SchedulingEntity policy lock old-inline/new-Arc/proc median {old:.2}/{new:.2}/{proc:.2} ns"
                );
            }
        }
    }
}
