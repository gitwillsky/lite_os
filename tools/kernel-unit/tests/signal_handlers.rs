#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production sighand lifecycle/pending owner. Architecture/frame/user-copy and graph membership
//! are explicit seams; transitions load the actual resource implementation.
extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; }
struct FaultAllocator;
// SAFETY: failure injection is thread-local; successful allocation/deallocation delegates to System.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_NEXT
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/arch/riscv64/signal_action.rs"]
mod signal_action;
mod arch {
    pub(crate) mod user {
        pub(crate) use crate::signal_action::SignalActionBytes;
    }
}
#[path = "../../../kernel/src/file/signalfd.rs"]
mod file;
mod sync {
    pub(crate) fn next_readiness_generation() -> u64 {
        static GENERATION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        GENERATION.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) type IrqMutexGuard<'a, T> = spin::MutexGuard<'a, T>;
}
mod memory {
    #[derive(Debug)]
    pub(crate) enum MemoryError {
        OutOfMemory,
    }
}
#[path = "../../../kernel/src/task"]
mod task {
    pub(crate) mod pid {
        pub(crate) const INIT_PID: usize = 1;
    }
    pub(crate) mod model {
        use alloc::sync::Arc;
        use spin::Mutex;
        mod signal_dequeue;
        mod signal_generation;
        mod signal_handlers;
        mod signal_info;
        pub(in crate::task) mod signal_pending;
        pub(in crate::task) use signal_info::PendingSignal;
        mod synchronous_fault;
        pub(in crate::task) use signal_handlers::{ChildSignalKind, ChildSignalPublication};
        use signal_handlers::{ProcessSignalState, SignalAction};
        use signal_pending::PendingSignals;
        pub(in crate::task) use signal_pending::PreparedSignalRecord;
        use signal_pending::RetiredSignals;
        pub(in crate::task) fn leader_signal_uid(handle: &u32) -> u32 {
            *handle
        }
        struct RecipientCredentials {
            uids: [u32; 3],
            user_namespace: Arc<crate::namespace::UserNamespace>,
        }
        fn leader_credential_snapshot(uid: &u32) -> RecipientCredentials {
            RecipientCredentials {
                uids: [*uid; 3],
                user_namespace: crate::namespace::initial_user(),
            }
        }

        fn normalize_signal_mask(mask: u64) -> u64 {
            mask & !(1 << 8 | 1 << 18)
        }
        fn signal_is_ignored(signal: usize, action: SignalAction) -> bool {
            action.handler == 1 || action.handler == 0 && matches!(signal, 17 | 18 | 23 | 28)
        }
        const RLIMIT_SIGPENDING: usize = 11;
        struct ResourceLimit {
            soft: u64,
        }
        struct Process {
            tgid: usize,
            signal_state: Mutex<ProcessSignalState>,
            leader_signal_mask: Mutex<Arc<Mutex<u64>>>,
            leader_credentials: u32,
        }
        struct Thread {
            signal_mask: Arc<Mutex<u64>>,
            pending_signals: Mutex<PendingSignals>,
        }
        pub(crate) struct TaskControlBlock {
            process: Arc<Process>,
            thread: Thread,
        }
        impl TaskControlBlock {
            fn is_kernel_thread(&self) -> bool {
                false
            }
            fn user_resource_owner(&self) -> (Arc<crate::namespace::UserNamespace>, u32) {
                (crate::namespace::initial_user(), 0)
            }
            fn resource_limit(&self, _: usize) -> Option<ResourceLimit> {
                Some(ResourceLimit { soft: u64::MAX })
            }
            pub(in crate::task) fn user_namespace(&self) -> Arc<crate::namespace::UserNamespace> {
                crate::namespace::initial_user()
            }
            pub(in crate::task) fn pid_namespace(&self) -> Arc<crate::namespace::PidNamespace> {
                crate::namespace::initial_pid()
            }
            fn is_namespace_init(&self) -> bool {
                self.process.tgid == 1
            }
            pub(in crate::task) fn set_mask(&self, mask: u64) {
                *self.thread.signal_mask.lock() = mask;
            }
            pub(in crate::task) fn accepts_process_signal(&self, signal: usize, _: bool) -> bool {
                *self.thread.signal_mask.lock() & (1 << (signal - 1)) == 0
            }
            pub(in crate::task) fn tid(&self) -> usize {
                self.tgid()
            }
            pub(in crate::task) fn tgid(&self) -> usize {
                self.process.tgid
            }
            fn process(&self) -> &Process {
                &self.process
            }
            fn thread(&self) -> &Thread {
                &self.thread
            }
            pub(in crate::task) fn new() -> Arc<Self> {
                Self::from_owner(ProcessSignalState::new(
                    ProcessSignalState::try_default_handlers().unwrap(),
                ))
            }
            fn from_owner(state: ProcessSignalState) -> Arc<Self> {
                let signal_mask = Arc::new(Mutex::new(0));
                Arc::new(Self {
                    process: Arc::new(Process {
                        tgid: 2,
                        signal_state: Mutex::new(state),
                        leader_signal_mask: Mutex::new(signal_mask.clone()),
                        leader_credentials: 581_751,
                    }),
                    thread: Thread {
                        signal_mask,
                        pending_signals: Mutex::new(PendingSignals::new()),
                    },
                })
            }
            pub(in crate::task) fn sibling(&self) -> Arc<Self> {
                Arc::new(Self {
                    process: self.process.clone(),
                    thread: Thread {
                        signal_mask: Arc::new(Mutex::new(0)),
                        pending_signals: Mutex::new(PendingSignals::new()),
                    },
                })
            }
            fn fork(&self, shared: bool) -> Arc<Self> {
                Self::from_owner(ProcessSignalState::new(
                    self.clone_signal_handlers(shared, false).unwrap(),
                ))
            }
            fn action(
                &self,
                signal: usize,
                handler: usize,
                flags: usize,
                mask: u64,
            ) -> SignalAction {
                self.signal_action_for_threads(
                    core::iter::empty(),
                    signal,
                    Some(SignalAction {
                        handler,
                        flags,
                        mask,
                        restorer: 0x4321,
                    }),
                    false,
                )
                .unwrap()
                .0
            }
            fn get(&self, signal: usize) -> SignalAction {
                self.signal_action_for_threads(core::iter::empty(), signal, None, false)
                    .unwrap()
                    .0
            }
        }

        #[test]
        fn traced_ignored_signal_keeps_a_delivery_event_without_forcing_init_disposition() {
            let task = TaskControlBlock::new();
            task.action(28, 1, 0, 0);
            let (queued, retired) = task
                .queue_signal(
                    core::iter::empty(),
                    28,
                    true,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            drop(retired);
            assert!(queued);
            let selected = {
                let mut pending = task.thread().pending_signals.lock();
                assert!(pending.requires_delivery(1 << 27));
                pending.take(1 << 27).unwrap()
            };
            let (signal, info) = selected.resolve(|_, _| false).unwrap();
            assert_eq!(signal, 28);
            assert!(
                !info.is_forced(),
                "ptrace provenance is not authority to bypass init's SIGKILL protection"
            );
            assert_eq!(task.get(28).handler, 1);
        }

        #[test]
        fn signalfd_reads_actual_reader_queues_and_pins_shared_sighand_source() {
            use crate::file::FileReadinessContext;
            let parent = TaskControlBlock::new();
            let sibling = parent.sibling();
            let shared_handlers = parent.fork(true);
            let independent = parent.fork(false);
            let source = parent.signal_source().unwrap();
            assert!(Arc::ptr_eq(&source, &sibling.signal_source().unwrap()));
            assert!(Arc::ptr_eq(
                &source,
                &shared_handlers.signal_source().unwrap()
            ));
            assert!(!Arc::ptr_eq(&source, &independent.signal_source().unwrap()));
            let descriptor =
                crate::file::SignalFd::try_new((1 << 9) | (1 << 8) | (1 << 18)).unwrap();
            assert_eq!(descriptor.mask(), 1 << 9);
            let duplicate = descriptor.clone();
            parent
                .queue_signal(
                    core::iter::empty(),
                    10,
                    false,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(parent.signal_pending(descriptor.mask()));
            assert!(!sibling.signal_pending(descriptor.mask()));
            assert!(!shared_handlers.signal_pending(descriptor.mask()));
            duplicate.replace_mask(1 << 11);
            assert!(!parent.signal_pending(descriptor.mask()));
            parent
                .queue_process_signal(
                    core::iter::empty(),
                    12,
                    false,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(sibling.signal_pending(duplicate.mask()));
            assert!(!shared_handlers.signal_pending(duplicate.mask()));
            let notification = sibling.read_signal_notification(duplicate.mask()).unwrap();
            assert_eq!(&notification[..4], &12u32.to_ne_bytes());
            assert!(notification[12..].iter().all(|byte| *byte == 0));
            assert!(!parent.signal_pending(descriptor.mask()));
            duplicate.replace_mask(1 << 9);
            assert!(parent.read_signal_notification(descriptor.mask()).is_some());
            let generation = source.generation();
            source.changed();
            assert!(source.generation() > generation);
        }

        #[test]
        fn canonical_dequeue_skips_deleted_timer_and_returns_next_actual_record() {
            crate::namespace::boot();
            let reader = TaskControlBlock::new();
            let stale = signal_pending::PreallocatedSignal::try_new(
                crate::namespace::initial_user(),
                591750,
                1,
                (2, 99),
            )
            .unwrap();
            let mut timer_record = stale.prepare(1);
            reader
                .queue_signal(
                    core::iter::empty(),
                    35,
                    false,
                    PendingSignal::timer(99, 0, 81),
                    &mut timer_record,
                )
                .unwrap();
            let info = PendingSignal::process_directed(42);
            let mut valid = PreparedSignalRecord::prepare(
                35,
                info,
                crate::namespace::initial_user(),
                591751,
                1,
            );
            assert!(valid.has_record());
            reader
                .queue_signal(core::iter::empty(), 35, false, info, &mut valid)
                .unwrap();
            let record = reader.read_signal_notification(1 << 34).unwrap();
            assert_eq!(&record[..4], &35u32.to_ne_bytes());
            assert_eq!(&record[8..12], &0i32.to_ne_bytes());
            assert_eq!(&record[12..16], &42u32.to_ne_bytes());
            assert!(reader.read_signal_notification(1 << 34).is_none());
            assert!(stale.prepare(2).has_record());
        }

        #[test]
        fn blocked_ignored_generation_remains_available_to_sigwait() {
            let task = TaskControlBlock::new();
            task.action(10, 1, 0, 0);
            task.action(17, 0, 0, 0);
            assert!(
                !task
                    .queue_signal(
                        core::iter::empty(),
                        10,
                        false,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default()
                    )
                    .unwrap()
                    .0
            );
            assert!(
                !task
                    .queue_process_signal(
                        core::iter::empty(),
                        17,
                        false,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default()
                    )
                    .unwrap()
                    .0
            );
            *task.thread().signal_mask.lock() = 1 << 9 | 1 << 16;
            assert!(
                task.queue_signal(
                    core::iter::empty(),
                    10,
                    false,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default()
                )
                .unwrap()
                .0
            );
            assert!(
                task.queue_process_signal(
                    core::iter::empty(),
                    17,
                    false,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default()
                )
                .unwrap()
                .0
            );
            assert_eq!(
                task.thread()
                    .pending_signals
                    .lock()
                    .take(u64::MAX)
                    .unwrap()
                    .signal,
                10
            );
            assert_eq!(
                task.process()
                    .signal_state
                    .lock()
                    .pending
                    .take(u64::MAX)
                    .unwrap()
                    .signal,
                17
            );
        }

        #[test]
        fn original_leader_mask_controls_pid_generation_after_execution_owner_drops() {
            let leader = TaskControlBlock::new();
            let worker = leader.sibling();
            leader.action(10, 1, 0, 0);
            leader.set_mask(1 << 9);
            let mask = Arc::downgrade(&leader.thread.signal_mask);
            drop(leader);
            assert!(
                mask.upgrade().is_some(),
                "Process retains actual scalar owner, not TCB"
            );
            assert!(
                worker
                    .queue_leader_process_signal(
                        core::iter::once(&worker),
                        10,
                        false,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default()
                    )
                    .unwrap()
                    .0
            );
            worker.set_mask(1 << 9);
            assert!(worker.read_signal_notification(1 << 9).is_some());
            *worker.process.leader_signal_mask.lock().lock() = 0;
            assert!(
                !worker
                    .queue_leader_process_signal(
                        core::iter::once(&worker),
                        10,
                        false,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default()
                    )
                    .unwrap()
                    .0
            );
            assert!(worker.read_signal_notification(1 << 9).is_none());
        }

        #[test]
        fn original_leader_uid_owns_the_actual_group_signal_record_charge() {
            crate::namespace::boot();
            let leader = TaskControlBlock::new();
            let worker = leader.sibling();
            drop(leader);
            let info = PendingSignal::process_directed(42);
            let record = worker.prepare_leader_signal_record(35, info);
            assert!(record.has_record());
            let blocked = PreparedSignalRecord::prepare(
                35,
                info,
                crate::namespace::initial_user(),
                worker.process.leader_credentials,
                1,
            );
            assert!(
                !blocked.has_record(),
                "the first record consumes the original leader UID budget"
            );
            drop(record);
            let available = PreparedSignalRecord::prepare(
                35,
                info,
                crate::namespace::initial_user(),
                worker.process.leader_credentials,
                1,
            );
            assert!(
                available.has_record(),
                "actual record Drop refunds the same owner"
            );
        }
        #[test]
        fn init_exemption_does_not_block_internal_group_exit() {
            let mut task = TaskControlBlock::new();
            let inner = Arc::get_mut(&mut task).unwrap();
            Arc::get_mut(&mut inner.process).unwrap().tgid = 1;
            assert!(
                !task
                    .queue_signal(
                        core::iter::empty(),
                        10,
                        false,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default()
                    )
                    .unwrap()
                    .0
            );
            *task.thread().signal_mask.lock() = 1 << 9;
            assert!(
                task.queue_signal(
                    core::iter::empty(),
                    10,
                    false,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default()
                )
                .unwrap()
                .0
            );
            task.queue_group_exit_signal();
            assert_ne!(task.thread().pending_signals.lock().bits & (1 << 8), 0);
        }
        #[test]
        fn forced_ancestor_kill_stop_only_bypass_container_default_disposition() {
            let owner = ProcessSignalState::try_default_handlers().unwrap();
            let mut handlers = owner.lock();
            for signal in [9, 19] {
                assert!(handlers.ignores_generation(signal, 0, true, false, false));
                assert!(!handlers.ignores_generation(signal, 0, true, false, true));
                assert!(handlers.ignores_generation(signal, 0, true, true, true));
            }
            assert!(handlers.ignores_generation(15, 0, true, false, true));
            handlers.replace(
                15,
                SignalAction {
                    handler: 0x1234,
                    ..SignalAction::default()
                },
            );
            assert!(!handlers.ignores_generation(15, 0, true, false, false));
            handlers.replace(
                15,
                SignalAction {
                    handler: 1,
                    ..SignalAction::default()
                },
            );
            assert!(handlers.ignores_generation(15, 0, true, false, true));
            assert!(!handlers.ignores_generation(15, 1 << 14, true, false, true));
        }
        #[test]
        fn flags_are_filtered_and_child_policy_distinguishes_default_ignore() {
            let task = TaskControlBlock::new();
            task.action(17, 0, 0, 0);
            *task.thread().signal_mask.lock() = 1 << 16;
            let default = task
                .publish_child_signal(
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    17,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(!default.autoreap && default.queued);
            task.action(17, 0, 3 | 0x400 | 0x800, 0);
            assert_eq!(task.get(17).flags, 3);
            let flags = task
                .publish_child_signal(
                    PendingSignal::default(),
                    ChildSignalKind::Job,
                    17,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(flags.autoreap && !flags.queued);
            task.action(17, 1, 0, 0);
            let ignored = task
                .publish_child_signal(
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    17,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(ignored.autoreap && !ignored.queued);
        }

        #[test]
        fn child_pending_publication_cannot_arrive_after_ignore_transition() {
            let task = TaskControlBlock::new();
            *task.thread().signal_mask.lock() = 1 << 16;
            task.action(17, 0x1234, 0, 0);
            let published = task
                .publish_child_signal(
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    17,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(published.queued && !published.autoreap);
            assert_ne!(task.process().signal_state.lock().pending.bits, 0);
            task.signal_action_for_threads(
                core::iter::empty(),
                17,
                Some(SignalAction {
                    handler: 1,
                    ..SignalAction::default()
                }),
                false,
            )
            .unwrap();
            // Only a scheduler wake is delayed. The publication token has no operation that queues.
            assert!(published.queued);
            assert_eq!(task.process().signal_state.lock().pending.bits, 0);
            let after = task
                .publish_child_signal(
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    17,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(after.autoreap && !after.queued);
        }

        #[test]
        fn clone_exit_signal_and_zero_do_not_inherit_sigchld_autoreap() {
            crate::namespace::boot();
            let task = TaskControlBlock::new();
            task.action(17, 1, 2, 0);
            task.action(10, 0x1234, 0, 0);
            let info = PendingSignal::child_exited(123, 7);
            let mut prepared = task.prepare_signal_record(10, info);
            let clone = task
                .publish_child_signal(info, ChildSignalKind::Exit, 10, &mut prepared)
                .unwrap();
            assert!(clone.queued && !clone.autoreap);
            let (signal, info, _) = task.take_pending_signal(u64::MAX).unwrap();
            assert_eq!(signal, 10);
            assert_eq!(
                i32::from_ne_bytes(info.encode(10)[24..28].try_into().unwrap()),
                7
            );
            let silent = task
                .publish_child_signal(
                    PendingSignal::child_exited(123, 8),
                    ChildSignalKind::Exit,
                    0,
                    &mut PreparedSignalRecord::default(),
                )
                .unwrap();
            assert!(!silent.queued && !silent.autoreap);
            assert!(task.take_pending_signal(u64::MAX).is_none());
        }

        #[test]
        fn clone_shares_only_dispositions_while_fork_copies() {
            let parent = TaskControlBlock::new();
            parent.action(10, 0x1234, 7, 0x8000);
            let shared = parent.fork(true);
            let copied = parent.fork(false);
            shared.action(10, 0x9999, 0, 0);
            assert_eq!(parent.get(10).handler, 0x9999);
            assert_eq!(copied.get(10).handler, 0x1234);
            parent.process().signal_state.lock().pending.queue(
                10,
                PendingSignal::default(),
                &mut PreparedSignalRecord::default(),
            );
            assert_eq!(shared.process().signal_state.lock().pending.bits, 0);
            assert_eq!(copied.process().signal_state.lock().pending.bits, 0);
        }

        #[test]
        fn exec_detaches_and_resets_fields_without_losing_pending_or_shared_actions() {
            let parent = TaskControlBlock::new();
            parent.action(10, 0x1234, 7, 0x8000);
            parent.action(12, 1, 0x8000_0000, 0x4000);
            let child = parent.fork(true);
            child.process().signal_state.lock().pending.queue(
                10,
                PendingSignal::default(),
                &mut PreparedSignalRecord::default(),
            );
            let detached = child.prepare_exec_signal_handlers().unwrap();
            assert!(detached.is_some());
            child.commit_exec_signal_handlers(detached);
            assert_eq!(parent.get(10).handler, 0x1234);
            assert_eq!(parent.get(12).flags, 0x8000_0000);
            let caught = child.get(10);
            assert_eq!(
                (caught.handler, caught.flags, caught.restorer, caught.mask),
                (0, 0, 0, 0)
            );
            let ignored = child.get(12);
            assert_eq!(
                (
                    ignored.handler,
                    ignored.flags,
                    ignored.restorer,
                    ignored.mask
                ),
                (1, 0, 0, 0)
            );
            assert_eq!(child.process().signal_state.lock().pending.bits, 1 << 9);
        }

        #[test]
        fn exec_copies_current_ignore_disposition_only_at_commit() {
            let parent = TaskControlBlock::new();
            parent.action(10, 1, 0, 0);
            parent.action(12, 0x1234, 0, 0);
            let child = parent.fork(true);
            let detached = child.prepare_exec_signal_handlers().unwrap();
            // External shared owner changes both directions during fallible image preparation.
            parent.action(10, 0x5678, 7, 0x8000);
            parent.action(12, 1, 7, 0x8000);
            child.process().signal_state.lock().pending.queue(
                10,
                PendingSignal::default(),
                &mut PreparedSignalRecord::default(),
            );
            child.commit_exec_signal_handlers(detached);
            assert_eq!(child.get(10).handler, 0);
            assert_eq!(child.get(12).handler, 1);
            assert_eq!(child.get(12).flags, 0);
            assert_eq!(child.process().signal_state.lock().pending.bits, 1 << 9);
            assert_eq!(parent.get(10).handler, 0x5678);
        }

        #[test]
        fn exec_oom_is_preflight_and_unique_exec_does_not_allocate() {
            let parent = TaskControlBlock::new();
            parent.action(10, 0x1234, 7, 0);
            let child = parent.fork(true);
            crate::FAIL_NEXT.with(|flag| flag.set(true));
            assert!(child.prepare_exec_signal_handlers().is_err());
            assert_eq!(child.get(10).handler, 0x1234);
            assert_eq!(parent.get(10).handler, 0x1234);
            child.retire_signal_handlers();
            crate::FAIL_NEXT.with(|flag| flag.set(true));
            let prepared = parent.prepare_exec_signal_handlers().unwrap();
            assert!(prepared.is_none());
            parent.commit_exec_signal_handlers(prepared);
            assert!(crate::FAIL_NEXT.with(|flag| flag.replace(false)));
        }

        #[test]
        fn final_exit_retires_table_despite_task_observers_and_other_process_keeps_owner() {
            let parent = TaskControlBlock::new();
            let owner = parent.clone_signal_handlers(true, false).unwrap();
            let weak = Arc::downgrade(&owner);
            drop(owner);
            let child = parent.fork(true);
            let observer = child.clone();
            child.retire_signal_handlers();
            assert!(
                observer
                    .signal_action_for_threads(core::iter::empty(), 10, None, false)
                    .is_err()
            );
            assert!(weak.upgrade().is_some());
            parent.retire_signal_handlers();
            assert!(weak.upgrade().is_none());
        }

        #[test]
        fn clone_clear_sighand_keeps_ignored_actions_and_does_not_change_parent() {
            let parent = TaskControlBlock::new();
            parent.action(10, 0x1234, 7, 3);
            parent.action(12, 1, 3, 5);
            let child = TaskControlBlock::from_owner(ProcessSignalState::new(
                parent.clone_signal_handlers(false, true).unwrap(),
            ));
            assert_eq!(child.get(10).handler, 0);
            assert_eq!(child.get(10).flags, 0);
            assert_eq!(child.get(12).handler, 1);
            assert_eq!(child.get(12).flags, 3);
            assert_eq!(parent.get(10).handler, 0x1234);
            child.action(12, 0, 0, 0);
            assert_eq!(parent.get(12).handler, 1);
        }

        #[test]
        fn ignore_transition_discards_all_group_pending_and_preserves_external_group_pending() {
            let task = TaskControlBlock::new();
            let sibling = task.sibling();
            let external = task.fork(true);
            task.action(10, 0x1234, 0, 0);
            task.action(17, 0x5678, 0, 0);
            for signal in [10, 17, 12] {
                task.process().signal_state.lock().pending.queue(
                    signal,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default(),
                );
                external.process().signal_state.lock().pending.queue(
                    signal,
                    PendingSignal::default(),
                    &mut PreparedSignalRecord::default(),
                );
                for thread in [&task, &sibling] {
                    thread.thread().pending_signals.lock().queue(
                        signal,
                        PendingSignal::default(),
                        &mut PreparedSignalRecord::default(),
                    );
                }
            }
            let threads = [&task, &sibling];
            for (signal, handler) in [(10, 1), (17, 0)] {
                task.signal_action_for_threads(
                    threads.into_iter(),
                    signal,
                    Some(SignalAction {
                        handler,
                        mask: u64::MAX,
                        ..SignalAction::default()
                    }),
                    false,
                )
                .unwrap();
            }
            for thread in [&task, &sibling] {
                assert_eq!(thread.thread().pending_signals.lock().bits, 1 << 11);
            }
            assert_eq!(task.process().signal_state.lock().pending.bits, 1 << 11);
            assert_eq!(
                external.process().signal_state.lock().pending.bits,
                1 << 9 | 1 << 16 | 1 << 11
            );
            assert_eq!(task.get(10).mask & (1 << 8 | 1 << 18), 0);
        }

        #[test]
        fn one_shot_selection_only_clears_handler_before_concurrent_installation() {
            let task = TaskControlBlock::new();
            task.action(10, 0x1234, 0x8000_0000, 0x8000);
            let selected = task
                .process()
                .signal_state
                .lock()
                .with_handlers(|handlers, _| handlers.select(10))
                .unwrap();
            assert_eq!(selected.handler, 0x1234);
            let reset = task.get(10);
            assert_eq!(
                (reset.handler, reset.flags, reset.restorer, reset.mask),
                (0, selected.flags, selected.restorer, selected.mask)
            );
            task.action(10, 0x9999, 0, 0);
            // The selected frame may fault or take arbitrary time; no delayed reset remains.
            assert_eq!(task.get(10).handler, 0x9999);
        }

        #[test]
        #[ignore = "manual CPU benchmark; reports production table-owner query cost"]
        fn benchmark_sighand_query() {
            let task = TaskControlBlock::new();
            for _ in 0..100_000 {
                std::hint::black_box(task.get(10));
            }
            let mut samples = [0.0; 5];
            for sample in &mut samples {
                let start = std::time::Instant::now();
                for _ in 0..1_000_000 {
                    std::hint::black_box(task.get(10));
                }
                *sample = start.elapsed().as_nanos() as f64 / 1_000_000.0;
            }
            samples.sort_by(f64::total_cmp);
            println!(
                "production group + shared handler locks: {:.2} ns/query",
                samples[2]
            );
        }
    }
    mod process_table {
        mod ptrace {
            pub(super) fn thread_traced(_: &super::ProcessGraph, _: usize) -> bool {
                false
            }
        }
        pub(in crate::task) mod timer_queue {
            pub(in crate::task) fn deliver_posix_signal(
                _: super::super::model::signal_pending::TimerSignalDelivery,
                _: &mut super::super::model::PendingSignal,
            ) -> bool {
                // 此 fixture 的外部 registry 输入为空：queued timer 已被删除，必须被拒绝。
                // 活跃 timer 的实际验证/重装由 posix_timer_lifetime 加载生产 owner 覆盖。
                false
            }
        }
        use super::model::{PendingSignal, TaskControlBlock};
        use alloc::{collections::BTreeMap, sync::Arc};
        enum ProcessState {
            Live(BTreeMap<usize, Arc<TaskControlBlock>>),
            Exited,
        }
        struct ProcessNode {
            process: Arc<()>,
            leader_credentials: u32,
            pid: crate::namespace::PidRegistration,
            parent: Option<usize>,
            parent_thread: Option<usize>,
            exit_signal: u8,
            exec_generation: u64,
            parent_exec_generation: u64,
            state: ProcessState,
        }
        fn protocol_pid() -> crate::namespace::PidRegistration {
            // 此 fixture 只验证 creator/reaper mask policy；实际 PID registration/projection 独立加载验证。
            crate::namespace::boot();
            crate::namespace::prepared_pid().take_registration()
        }
        struct ProcessGraph {
            nodes: BTreeMap<usize, ProcessNode>,
            reaper: (usize, usize),
        }
        struct ProcessTable {
            graph: spin::Mutex<ProcessGraph>,
        }
        static PROCESS_TABLE: ProcessTable = ProcessTable {
            graph: spin::Mutex::new(ProcessGraph {
                nodes: BTreeMap::new(),
                reaper: (1, 1),
            }),
        };
        mod pid_namespace {
            // namespace ancestry 输入 seam；此 fixture 的 graph 仅装配一个显式 root reaper。
            pub(super) fn nearest_reaper(graph: &super::ProcessGraph, _: usize) -> (usize, usize) {
                graph.reaper
            }
        }
        mod signal {
            use super::*;
            fn wake_process_signal_waiter(_: usize) -> bool {
                true
            }
            fn interrupt_waiting_task(_: &Arc<TaskControlBlock>) -> bool {
                true
            }
            mod child_notification;
            pub(super) use child_notification::{
                ChildNotificationScope, PreparedChildNotifications, stage_child_signal,
            };
            pub(super) fn prepared_child(
                graph: &ProcessGraph,
                child: usize,
            ) -> PreparedChildNotifications {
                let mut prepared = PreparedChildNotifications::capture(
                    graph,
                    child,
                    ChildNotificationScope::Parent,
                );
                prepared.prepare();
                prepared
            }
        }
        #[test]
        fn child_generation_follows_creator_and_reaper_mask_instead_of_first_sibling() {
            use super::model::ChildSignalKind;
            let leader = TaskControlBlock::new();
            let creator = leader.sibling();
            let init = TaskControlBlock::new();
            let child = ProcessNode {
                process: Arc::new(()),
                leader_credentials: 0,
                pid: protocol_pid(),
                parent: Some(2),
                parent_thread: Some(20),
                exit_signal: 17,
                exec_generation: 0,
                parent_exec_generation: 0,
                state: ProcessState::Live(BTreeMap::new()),
            };
            let mut graph = ProcessGraph {
                reaper: (1, 1),
                nodes: BTreeMap::from([
                    (
                        1,
                        ProcessNode {
                            process: Arc::new(()),
                            leader_credentials: 0,
                            pid: protocol_pid(),
                            parent: None,
                            parent_thread: None,
                            exit_signal: 17,
                            exec_generation: 0,
                            parent_exec_generation: 0,
                            state: ProcessState::Live(BTreeMap::from([(1, init.clone())])),
                        },
                    ),
                    (
                        2,
                        ProcessNode {
                            process: Arc::new(()),
                            leader_credentials: 0,
                            pid: protocol_pid(),
                            parent: None,
                            parent_thread: None,
                            exit_signal: 17,
                            exec_generation: 0,
                            parent_exec_generation: 0,
                            state: ProcessState::Live(BTreeMap::from([
                                (2, leader.clone()),
                                (20, creator.clone()),
                            ])),
                        },
                    ),
                    (3, child),
                ]),
            };
            for kind in [ChildSignalKind::Exit, ChildSignalKind::Job] {
                leader.set_mask(0);
                creator.set_mask(1 << 16);
                assert!(
                    signal::stage_child_signal(
                        &graph,
                        3,
                        PendingSignal::default(),
                        kind,
                        &mut signal::prepared_child(&graph, 3)
                    )
                    .unwrap()
                    .queued
                );
                leader.set_mask(1 << 16);
                creator.set_mask(0);
                assert!(
                    !signal::stage_child_signal(
                        &graph,
                        3,
                        PendingSignal::default(),
                        kind,
                        &mut signal::prepared_child(&graph, 3)
                    )
                    .unwrap()
                    .queued
                );
                // Creator exit redirects the same canonical edge to the surviving leader.
                graph.nodes.get_mut(&3).unwrap().parent_thread = Some(2);
                assert!(
                    signal::stage_child_signal(
                        &graph,
                        3,
                        PendingSignal::default(),
                        kind,
                        &mut signal::prepared_child(&graph, 3)
                    )
                    .unwrap()
                    .queued
                );
                graph.nodes.get_mut(&3).unwrap().parent_thread = Some(20);
            }
            // Adoption changes both canonical relation facts, using init's mask.
            let child = graph.nodes.get_mut(&3).unwrap();
            child.parent = Some(1);
            child.parent_thread = Some(1);
            init.set_mask(1 << 16);
            assert!(
                signal::stage_child_signal(
                    &graph,
                    3,
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    &mut signal::prepared_child(&graph, 3),
                )
                .unwrap()
                .queued
            );
            init.set_mask(0);
            assert!(
                !signal::stage_child_signal(
                    &graph,
                    3,
                    PendingSignal::default(),
                    ChildSignalKind::Exit,
                    &mut signal::prepared_child(&graph, 3),
                )
                .unwrap()
                .queued
            );
            // 实际 relation 保存 custom signal；parent exec 只把 notification 改成 SIGCHLD。
            while init.take_pending_signal(u64::MAX).is_some() {}
            graph.nodes.get_mut(&3).unwrap().exit_signal = 10;
            init.set_mask((1 << 9) | (1 << 16));
            let mut prepared = signal::PreparedChildNotifications::capture(
                &graph,
                3,
                signal::ChildNotificationScope::ExitAdoption,
            );
            prepared.prepare();
            assert!(prepared.matches(&graph, 3, signal::ChildNotificationScope::ExitAdoption));
            graph.nodes.get_mut(&1).unwrap().exec_generation = 1;
            assert!(!prepared.matches(&graph, 3, signal::ChildNotificationScope::ExitAdoption));
            let mut current = signal::PreparedChildNotifications::capture(
                &graph,
                3,
                signal::ChildNotificationScope::ExitAdoption,
            );
            current.prepare();
            assert!(
                signal::stage_child_signal(
                    &graph,
                    3,
                    PendingSignal::child_exited(3, 7),
                    ChildSignalKind::Exit,
                    &mut current,
                )
                .unwrap()
                .queued
            );
            assert_eq!(graph.nodes[&3].exit_signal, 10);
            assert_eq!(init.take_pending_signal(u64::MAX).unwrap().0, 17);
        }
    }
    pub(crate) mod processor {
        pub(in crate::task) fn request_task_reschedule(_: &super::model::TaskControlBlock) {}
    }
}
