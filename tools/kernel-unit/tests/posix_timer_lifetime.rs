#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 POSIX schedule、prepared AVL node 与 pending/ucounts owner 的组合回归。
//! 时钟、creator/graph 装配及外层非 POSIX timer registry 字段是显式输入 seam。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

static NOW: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[path = "."]
pub(crate) mod execution {
    pub(crate) mod timer {
        pub(crate) fn get_time_ns() -> u64 {
            crate::NOW.load(core::sync::atomic::Ordering::Relaxed)
        }
        pub(crate) fn realtime_deadline_to_monotonic_ns(value: u64) -> u64 {
            value
        }
    }
    #[path = "../../../kernel/src/execution/task"]
    pub(crate) mod task {
        use alloc::sync::Arc;
        pub(crate) use model::PendingSignal;
        struct TaskControlBlock;
        struct ResourceLimit {
            soft: u64,
        }
        impl TaskControlBlock {
            fn user_resource_owner(&self) -> (Arc<crate::namespace::UserNamespace>, u32) {
                (crate::namespace::initial_user(), 591602)
            }
            fn resource_limit(&self, _: usize) -> Option<ResourceLimit> {
                Some(ResourceLimit { soft: 5 })
            }
        }
        static CREATOR: spin::Once<Arc<TaskControlBlock>> = spin::Once::new();
        fn current_task() -> Option<Arc<TaskControlBlock>> {
            CREATOR.get().cloned()
        }
        #[path = "model"]
        mod model {
            mod signal_info;
            mod signal_pending;
            pub(crate) use signal_info::PendingSignal;
            pub(in crate::execution::task) use signal_pending::{
                PreallocatedSignal, PreparedSignalRecord, TimerSignalDelivery,
            };
            pub(in crate::execution::task) const RLIMIT_SIGPENDING: usize = 11;
            pub(in crate::execution::task) struct Pending(
                spin::Mutex<signal_pending::PendingSignals>,
            );
            impl Pending {
                pub(in crate::execution::task) fn new() -> Self {
                    Self(spin::Mutex::new(signal_pending::PendingSignals::new()))
                }
                pub(in crate::execution::task) fn publish(
                    &self,
                    signal: usize,
                    info: PendingSignal,
                    prepared: &mut PreparedSignalRecord,
                ) {
                    self.0.lock().queue(signal, info, prepared);
                }
                pub(in crate::execution::task) fn consume(
                    &self,
                    mask: u64,
                ) -> Option<(usize, PendingSignal)> {
                    let selected = self.0.lock().take(mask);
                    selected.and_then(|selected| {
                        selected.resolve(super::process_table::timer_queue::deliver_posix_signal)
                    })
                }
                pub(in crate::execution::task) fn bits(&self) -> u64 {
                    self.0.lock().bits
                }
                pub(in crate::execution::task) fn clear(&self) {
                    let retired = self.0.lock().discard(u64::MAX);
                    drop(retired);
                }
            }
        }
        #[path = "process_table"]
        mod process_table {
            struct ProcessTable {
                timers: spin::Mutex<timer_queue::TimerQueue>,
            }
            static PROCESS_TABLE: ProcessTable = ProcessTable {
                timers: spin::Mutex::new(timer_queue::TimerQueue::new()),
            };
            #[path = "timer_queue"]
            pub(in crate::execution::task) mod timer_queue {
                use crate::execution::task::model::{Pending, PendingSignal};
                use crate::fallible_tree::{FallibleMap, VacantEntry};
                use alloc::sync::{Arc, Weak};
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                enum TimerError {
                    NotFound,
                    OutOfMemory,
                    Exhausted,
                }
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                enum PosixTimerClock {
                    Realtime,
                    Monotonic,
                    Boottime,
                }
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                enum PosixTimerNotification {
                    Default,
                    None,
                    Process {
                        signal: usize,
                        value: u64,
                    },
                    Thread {
                        tid: usize,
                        signal: usize,
                        value: u64,
                    },
                }
                #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
                enum TimerIdentity {
                    Real(usize),
                    Posix(usize, i32),
                }
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                struct TimerSetting {
                    remaining_ns: u64,
                    interval_ns: u64,
                }
                #[derive(Clone, Copy)]
                struct RealTimer {
                    next_expiration_ns: Option<u64>,
                    interval_ns: u64,
                }
                struct KernelTimer;
                type DeadlineEntry = VacantEntry<(u64, TimerIdentity), Option<Weak<KernelTimer>>>;
                #[derive(Default)]
                struct RetiredTimers {
                    deadlines: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
                }
                mod period;
                use period::next_period;
                mod preparation;
                mod preparation_policy;
                use preparation::{PreparedPosixCreate, PreparedPosixReplacement};
                mod posix_creation;
                mod posix_schedule;
                pub(in crate::execution::task) use posix_schedule::deliver_posix_signal;
                use posix_schedule::{PosixSchedule, PosixTimer};
                pub(super) struct ExpiredPosixTimer {
                    tgid: usize,
                    id: i32,
                    notification: PosixTimerNotification,
                    sequence: u64,
                    owner: Arc<crate::execution::task::model::PreallocatedSignal>,
                    thread_target: Option<Arc<crate::namespace::PidIdentity>>,
                }
                enum ExpiredTimer {
                    Posix(ExpiredPosixTimer),
                }
                pub(super) struct TimerQueue {
                    posix_timers: FallibleMap<(usize, i32), PosixTimer>,
                    deadline_index: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
                }
                impl TimerQueue {
                    pub(super) const fn new() -> Self {
                        Self {
                            posix_timers: FallibleMap::new(),
                            deadline_index: FallibleMap::new(),
                        }
                    }
                }
                static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
                fn boot() {
                    crate::namespace::boot();
                    crate::execution::task::CREATOR
                        .call_once(|| Arc::new(crate::execution::task::TaskControlBlock));
                    crate::NOW.store(0, core::sync::atomic::Ordering::Relaxed);
                }
                fn create(id: i32, notification: PosixTimerNotification) {
                    let mut prepared = PreparedPosixCreate::prepare(
                        90,
                        id,
                        PosixTimerClock::Monotonic,
                        notification,
                        None,
                    )
                    .unwrap();
                    assert!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .commit_posix_create(&mut prepared)
                    );
                }
                fn set(id: i32, value: u64, interval: u64) -> TimerSetting {
                    let needed = super::PROCESS_TABLE
                        .timers
                        .lock()
                        .posix_deadline_needed(90, id, value != 0)
                        .unwrap();
                    let mut prepared = PreparedPosixReplacement::prepare(
                        90, id, value, interval, true, needed, None,
                    )
                    .unwrap();
                    let (previous, retired) = super::PROCESS_TABLE
                        .timers
                        .lock()
                        .replace_posix(
                            &mut prepared,
                            crate::execution::timer::get_time_ns(),
                            crate::namespace::TimeOffsets::ZERO,
                        )
                        .unwrap()
                        .unwrap();
                    drop(retired);
                    previous
                }
                fn expire(id: i32) -> ExpiredPosixTimer {
                    let mut timers = super::PROCESS_TABLE.timers.lock();
                    let expiration = match timers.posix_timers.get(&(90, id)).unwrap().schedule {
                        PosixSchedule::Armed { expiration } => expiration,
                        _ => panic!("actual armed timer"),
                    };
                    let deadline = timers
                        .deadline_index
                        .take_entry(&(expiration, TimerIdentity::Posix(90, id)))
                        .unwrap();
                    let ExpiredTimer::Posix(expired) =
                        timers.expire_posix((90, id), expiration, deadline);
                    expired
                }
                fn publish(pending: &Pending, expired: &ExpiredPosixTimer) {
                    let mut prepared = super::PROCESS_TABLE
                        .timers
                        .lock()
                        .prepare_posix_notification(expired)
                        .unwrap();
                    if prepared.has_record() {
                        let PosixTimerNotification::Process { signal, value } =
                            expired.notification
                        else {
                            panic!("process signal fixture")
                        };
                        pending.publish(
                            signal,
                            PendingSignal::timer(expired.id, 0, value),
                            &mut prepared,
                        );
                    }
                }
                fn remove(id: i32) {
                    // 外层 graph/delete 输入 seam：真正 composite AVL entry 移出 guard 后退休。
                    let entry = {
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix_timers
                            .take_entry(&(90, id))
                            .unwrap()
                    };
                    assert!(!matches!(
                        entry.value().schedule,
                        PosixSchedule::Armed { .. }
                    ));
                    drop(entry);
                }
                fn full() {
                    assert!(matches!(
                        crate::namespace::RlimitCharge::acquire(
                            crate::namespace::initial_user(),
                            591602,
                            crate::namespace::RlimitResource::PendingSignals,
                            1,
                            5,
                            false
                        ),
                        Err(crate::namespace::NamespaceError::QuotaLimit)
                    ));
                }
                #[test]
                fn periodic_notification_pauses_until_actual_consumption_then_reuses_deadline_node()
                {
                    let _serial = SERIAL.lock().unwrap();
                    boot();
                    create(
                        0,
                        PosixTimerNotification::Process {
                            signal: 35,
                            value: 42,
                        },
                    );
                    let pending = Pending::new();
                    set(0, 100, 10);
                    let expired = expire(0);
                    assert!(super::PROCESS_TABLE.timers.lock().deadline_index.is_empty());
                    publish(&pending, &expired);
                    drop(expired);
                    crate::NOW.store(145, core::sync::atomic::Ordering::Relaxed);
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix(90, 0, 145)
                            .unwrap()
                            .remaining_ns,
                        5
                    );
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix_overrun(90, 0)
                            .unwrap(),
                        0
                    );
                    crate::allocation_failure::fail_next();
                    let (number, info) = pending.consume(1 << 34).unwrap();
                    assert_eq!(number, 35);
                    assert_eq!(&info.encode(35)[20..24], &4i32.to_ne_bytes());
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix_overrun(90, 0)
                            .unwrap(),
                        4
                    );
                    assert!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .deadline_index
                            .contains_key(&(150, TimerIdentity::Posix(90, 0)))
                    );
                    assert!(!crate::allocation_failure::was_consumed());
                    set(0, 0, 0);
                    remove(0);
                }
                #[test]
                fn replacement_invalidates_old_notification_and_new_expiry_refreshes_same_pending_record()
                 {
                    let _serial = SERIAL.lock().unwrap();
                    boot();
                    create(
                        0,
                        PosixTimerNotification::Process {
                            signal: 35,
                            value: 43,
                        },
                    );
                    let pending = Pending::new();
                    set(0, 100, 10);
                    let first = expire(0);
                    publish(&pending, &first);
                    drop(first);
                    set(0, 200, 20);
                    assert!(pending.consume(1 << 34).is_none());
                    assert_eq!(pending.bits(), 0);
                    let second = expire(0);
                    publish(&pending, &second);
                    drop(second);
                    set(0, 300, 30);
                    let third = expire(0);
                    publish(&pending, &third);
                    drop(third);
                    crate::NOW.store(365, core::sync::atomic::Ordering::Relaxed);
                    let (_, info) = pending.consume(1 << 34).unwrap();
                    assert_eq!(&info.encode(35)[20..24], &2i32.to_ne_bytes());
                    assert_eq!(pending.bits(), 0);
                    assert!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .deadline_index
                            .contains_key(&(390, TimerIdentity::Posix(90, 0)))
                    );
                    set(0, 0, 0);
                    remove(0);
                }
                #[test]
                fn deleted_timer_notification_cannot_rearm_same_id_new_timer_and_charge_waits_for_record()
                 {
                    let _serial = SERIAL.lock().unwrap();
                    boot();
                    for id in 0..5 {
                        create(
                            id,
                            PosixTimerNotification::Process {
                                signal: 35,
                                value: id as u64,
                            },
                        );
                    }
                    full();
                    let pending = Pending::new();
                    set(0, 100, 10);
                    let expired = expire(0);
                    publish(&pending, &expired);
                    drop(expired);
                    remove(0);
                    full();
                    assert!(pending.consume(1 << 34).is_none());
                    create(
                        0,
                        PosixTimerNotification::Process {
                            signal: 35,
                            value: 90,
                        },
                    );
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix(90, 0, 0)
                            .unwrap()
                            .remaining_ns,
                        0
                    );
                    for id in 0..5 {
                        remove(id);
                    }
                }
                #[test]
                fn silent_timers_charge_at_creation_and_lazy_interval_has_no_active_deadline() {
                    let _serial = SERIAL.lock().unwrap();
                    boot();
                    for id in 0..5 {
                        create(id, PosixTimerNotification::None);
                    }
                    full();
                    let result = PreparedPosixCreate::prepare(
                        90,
                        5,
                        PosixTimerClock::Monotonic,
                        PosixTimerNotification::None,
                        None,
                    );
                    assert!(matches!(result, Err(TimerError::Exhausted)));
                    set(0, 100, 10);
                    assert!(super::PROCESS_TABLE.timers.lock().deadline_index.is_empty());
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix(90, 0, 145)
                            .unwrap()
                            .remaining_ns,
                        5
                    );
                    set(0, 100, 0);
                    assert_eq!(
                        super::PROCESS_TABLE
                            .timers
                            .lock()
                            .posix(90, 0, 145)
                            .unwrap()
                            .remaining_ns,
                        0
                    );
                    for id in 0..5 {
                        remove(id);
                    }
                }
                #[test]
                fn ignored_periodic_notification_reuses_record_when_action_is_restored() {
                    let _serial = SERIAL.lock().unwrap();
                    boot();
                    create(
                        0,
                        PosixTimerNotification::Process {
                            signal: 35,
                            value: 44,
                        },
                    );
                    let pending = Pending::new();
                    set(0, 100, 10);
                    let expired = expire(0);
                    let mut prepared = super::PROCESS_TABLE
                        .timers
                        .lock()
                        .prepare_posix_notification(&expired)
                        .unwrap();
                    super::PROCESS_TABLE
                        .timers
                        .lock()
                        .ignore_posix_expiry(&expired);
                    prepared.recycle_timer();
                    drop(expired);
                    assert!(super::PROCESS_TABLE.timers.lock().deadline_index.is_empty());
                    crate::NOW.store(145, core::sync::atomic::Ordering::Relaxed);
                    let restored = super::PROCESS_TABLE
                        .timers
                        .lock()
                        .resume_ignored_posix(90, 35, -1)
                        .unwrap();
                    publish(&pending, &restored);
                    drop(restored);
                    assert_eq!(
                        &pending.consume(1 << 34).unwrap().1.encode(35)[20..24],
                        &4i32.to_ne_bytes()
                    );
                    set(0, 0, 0);
                    remove(0);
                }
            }
        }
    }
}
