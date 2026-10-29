#![feature(allocator_ext)]
#![allow(dead_code)]
//! 执行生产 timer sleep、TaskEvent 和 AVL；只替换 clock、IRQ lock 与 scheduler target。

extern crate alloc;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static REJECT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    static DEADLINE_NODE: Cell<usize> = const { Cell::new(0) };
    static EXPIRATIONS: Cell<usize> = const { Cell::new(0) };
}

struct FaultAllocator;
// SAFETY: 正常请求完整委托 System；目标线程的故障窗口只拒绝 allocation，不改变 deallocation。
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if REJECT_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
            ALLOCATION_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
            return core::ptr::null_mut();
        }
        // SAFETY: layout 原样转交匹配的 System allocator。
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: 成功 allocation 均来自相同 System 和原 layout。
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

mod memory {
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum MemoryError {
        OutOfMemory,
    }
}

mod id {
    pub(crate) fn next_runtime_object_id() -> u64 {
        42
    }
}

#[test]
fn repeated_sleep_reuses_deadline_and_event_under_total_allocation_failure() {
    crate::execution::sync::warm_target();
    let mut waiter =
        crate::execution::task::process_table::timer_queue::KernelDeadlineWait::new().unwrap();
    REJECT_ALLOCATIONS.set(true);
    let mut control = alloc::vec::Vec::<u64>::new();
    let control_failed = control.try_reserve_exact(2).is_err();
    ALLOCATION_ATTEMPTS.set(0);
    for deadline in 1..=100 {
        waiter.sleep_until(deadline);
    }
    let attempts = ALLOCATION_ATTEMPTS.get();
    REJECT_ALLOCATIONS.set(false);
    assert!(control_failed, "allocator failure must actually be armed");
    assert_eq!(attempts, 0);
    assert_eq!(EXPIRATIONS.get(), 100);
    // 已到期 deadline 不发布 node，也不等待新事件。
    waiter.sleep_until(0);
    assert_eq!(EXPIRATIONS.get(), 100);
}

#[path = "."]
pub(crate) mod execution {
    pub(crate) mod timer {
        pub(crate) fn get_time_ns() -> u64 {
            0
        }
    }
    #[path = "../../../kernel/src/execution/sync"]
    pub(crate) mod sync {
        pub(crate) struct IrqMutex<T>(spin::Mutex<T>);
        impl<T> IrqMutex<T> {
            pub(crate) const fn new(value: T) -> Self {
                Self(spin::Mutex::new(value))
            }
            pub(crate) fn lock(&self) -> spin::MutexGuard<'_, T> {
                self.0.lock()
            }
        }
        mod wait_completion;
        pub(crate) use wait_completion::WaitCompletion;
        mod task_event;
        pub(crate) use task_event::TaskEvent;
        pub(crate) use task_wait::WaitResult;

        mod task_wait;
        mod target {
            use super::{
                WaitCompletion,
                task_wait::{TaskWaitKey, TaskWaitMode, TaskWaitTarget, WaitResult},
            };
            use alloc::sync::Arc;

            struct Target;
            impl TaskWaitTarget for Target {
                fn sleep(
                    self: Arc<Self>,
                    completion: &WaitCompletion,
                    key: TaskWaitKey,
                ) -> WaitResult {
                    assert_eq!(
                        key.mode,
                        TaskWaitMode::Idle,
                        "daemon deadline sleep must not contribute to system load"
                    );
                    assert!(completion.begin_arming());
                    // 定时器在 scheduler arming 期间到期；真实 TaskEvent 必须保留 pending 并完成握手。
                    crate::execution::task::process_table::timer_queue::expire();
                    assert!(completion.finish_arming());
                    WaitResult::Woken
                }
                fn wake(self: Arc<Self>, _: TaskWaitKey) {
                    panic!("arming completion must not issue a sleeping wake");
                }
            }
            static TARGET: spin::Once<Arc<dyn TaskWaitTarget>> = spin::Once::new();
            pub(super) fn factory() -> Option<Arc<dyn TaskWaitTarget>> {
                Some(TARGET.call_once(|| Arc::new(Target)).clone())
            }
        }
        pub(crate) fn warm_target() {
            let _ = target::factory();
            static INSTALL: std::sync::Once = std::sync::Once::new();
            INSTALL.call_once(|| task_wait::install_wait_target_factory(target::factory));
        }
    }
    #[path = "../../../kernel/src/execution/task"]
    pub(crate) mod task {
        #[path = "process_table"]
        pub(crate) mod process_table {
            // clock/expiry drive 为测试注入；deadline index 使用生产 AVL 和相同 key/payload。
            struct ProcessTable {
                timers: crate::execution::sync::IrqMutex<timer_queue::TimerQueue>,
            }
            static PROCESS_TABLE: ProcessTable = ProcessTable {
                timers: crate::execution::sync::IrqMutex::new(timer_queue::TimerQueue::new()),
            };

            pub(crate) mod timer_queue {
                use crate::fallible_tree::{FallibleMap, VacantEntry};
                use alloc::sync::Weak;

                #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
                pub(super) enum TimerIdentity {
                    Kernel(u64),
                }
                type DeadlineEntry = VacantEntry<(u64, TimerIdentity), Option<Weak<KernelTimer>>>;
                pub(super) struct TimerQueue {
                    deadline_index: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
                }
                impl TimerQueue {
                    pub(super) const fn new() -> Self {
                        Self {
                            deadline_index: FallibleMap::new(),
                        }
                    }
                }
                mod kernel_wait;
                pub(crate) use kernel_wait::KernelDeadlineWait;
                use kernel_wait::{KernelTimer, KernelTimerExpiration};
                pub(super) struct TimerExpirationRetirement {
                    entry: Option<DeadlineEntry>,
                }
                impl TimerExpirationRetirement {
                    fn from_deadline(entry: Option<DeadlineEntry>) -> Self {
                        Self { entry }
                    }
                }
                pub(super) enum ExpiredTimer {
                    Kernel(KernelTimerExpiration),
                    Silent(TimerExpirationRetirement),
                }

                pub(crate) fn expire() {
                    let entry = {
                        let mut queue = super::PROCESS_TABLE.timers.lock();
                        let key = *queue.deadline_index.first_key_value().unwrap().0;
                        queue.deadline_index.take_entry(&key).unwrap()
                    };
                    let pointer = entry.value() as *const _ as usize;
                    crate::DEADLINE_NODE.with(|saved| {
                        if saved.get() == 0 {
                            saved.set(pointer);
                        }
                        assert_eq!(saved.get(), pointer, "deadline storage must be reused");
                    });
                    match KernelTimerExpiration::from_entry(entry) {
                        ExpiredTimer::Kernel(expired) => expired.complete(),
                        ExpiredTimer::Silent(_) => panic!("live sleep lost timer owner"),
                    }
                    crate::EXPIRATIONS.with(|count| count.set(count.get() + 1));
                }
            }
        }
    }
}
