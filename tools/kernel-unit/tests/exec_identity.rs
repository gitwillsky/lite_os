#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production exec graph/PID registration/AVL transaction；TCB execution 与 IRQ adapter 是窄 seam。
extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
thread_local! { static FAIL_NEXT: Cell<bool> = const { Cell::new(false) }; }
struct FaultAllocator;
// SAFETY: thread-local allocation failure; successful allocations delegate to System.
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
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/sync.rs"]
mod sync;
#[path = "../src/interruptible_event_target.rs"]
mod target;
mod memory {
    #[derive(Debug)]
    pub(crate) enum ElfLoadError {
        OutOfMemory,
    }
}
mod fs {
    pub(crate) mod pidfs {
        #[derive(Default)]
        pub(crate) struct PidFdSource(pub(crate) core::sync::atomic::AtomicUsize);
        impl PidFdSource {
            pub(crate) fn notify(&self) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::Release);
            }
        }
    }
}
#[path = "../../../kernel/src/task"]
mod task {
    use crate::fallible_tree::FallibleMap;
    use alloc::sync::Arc;
    use core::sync::atomic::Ordering;
    #[allow(
        unused_imports,
        reason = "focused identity fixture consumes a subset of the actual PID facade"
    )]
    pub(crate) mod pid;
    use pid::{PidIdentity, PidLinkSlot, PidRegistration, PreparedPid};
    pub(crate) struct TaskControlBlock {
        tgid: Arc<PidIdentity>,
        tid: spin::Mutex<Arc<PidIdentity>>,
        killed: core::sync::atomic::AtomicBool,
        retirements: Arc<model::thread_retirement::ThreadRetirements>,
    }
    #[path = "model"]
    mod model {
        pub(super) mod thread_retirement;
        pub(super) use super::TaskControlBlock;
        pub(super) fn initial_retirements() -> alloc::sync::Arc<thread_retirement::ThreadRetirements>
        {
            alloc::sync::Arc::new(thread_retirement::ThreadRetirements::new())
        }
        impl TaskControlBlock {
            fn process(&self) -> &Self {
                self
            }
        }
        #[test]
        fn actual_final_cleanup_token_alone_publishes_process_completion() {
            for original_completes_first in [false, true] {
                let lifetime = initial_retirements();
                lifetime.publish_thread();
                let original = lifetime.retirement();
                let sibling = lifetime.retirement();
                if original_completes_first {
                    assert!(!original.complete());
                    assert!(sibling.complete());
                } else {
                    assert!(!sibling.complete());
                    assert!(original.complete());
                }
            }
        }
        #[derive(Debug)]
        pub(crate) enum ExecError {
            Image(crate::memory::ElfLoadError),
            Busy,
            Interrupted,
            FatalInterrupted,
        }
    }
    pub(crate) mod processor {
        pub(crate) fn continue_stopped_task(_: alloc::sync::Arc<super::TaskControlBlock>) {}
        pub(crate) fn request_task_reschedule(_: &alloc::sync::Arc<super::TaskControlBlock>) {}
    }
    static CREDENTIAL_GUARD: crate::sync::TaskMutex<()> = crate::sync::TaskMutex::new(());
    impl TaskControlBlock {
        fn credential_guard(&self) -> &crate::sync::TaskMutex<()> {
            &CREDENTIAL_GUARD
        }
        fn queue_group_exit_signal(&self) {
            self.killed.store(true, Ordering::Release);
        }
        fn tgid(&self) -> usize {
            self.tgid.key()
        }
        fn tid(&self) -> usize {
            self.tid.lock().key()
        }
        fn promote_exec_identity(&self) -> Arc<PidIdentity> {
            core::mem::replace(&mut *self.tid.lock(), self.tgid.clone())
        }
    }
    #[path = "process_table"]
    mod process_table {
        use super::*;
        enum ProcessState {
            Live(FallibleMap<usize, Arc<TaskControlBlock>>),
            Zombie,
        }
        struct ProcessNode {
            pid: PidRegistration,
            state: ProcessState,
            parent_thread: Option<usize>,
            pdeath_cursor: usize,
            exec_generation: u64,
            exit_signal: u8,
            group_exit: Option<u8>,
            exec: Option<exec_drain::ExecState>,
            job_control: JobControlState,
        }
        struct ThreadIndex {
            pid: PidRegistration,
            pidfd_source: alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
            tgid: usize,
            created_children: FallibleMap<usize, ()>,
        }
        struct ProcessGraph {
            nodes: FallibleMap<usize, ProcessNode>,
            threads: FallibleMap<usize, ThreadIndex>,
        }
        struct ProcessTable {
            graph: spin::Mutex<ProcessGraph>,
        }
        static PROCESS_TABLE: ProcessTable = ProcessTable {
            graph: spin::Mutex::new(ProcessGraph {
                nodes: FallibleMap::new(),
                threads: FallibleMap::new(),
            }),
        };
        enum JobControlState {
            Running,
        }
        mod signal {
            pub(super) fn interrupt_waiting_task(_: &super::TaskControlBlock) {}
        }
        mod exec_drain;
        mod exec_identity;
        #[cfg(test)]
        mod tests {
            use super::*;
            static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
            fn prepared(key: usize) -> PreparedPid {
                crate::namespace::boot();
                let mut prepared = PreparedPid::try_new(
                    key,
                    crate::namespace::initial_pid(),
                    &[],
                    &crate::namespace::root_authority(),
                )
                .unwrap();
                prepared.publish().unwrap();
                prepared
            }
            fn setup(tid: usize) -> Arc<TaskControlBlock> {
                let mut process_pid = prepared(11);
                let mut thread_pid = if tid != 11 { Some(prepared(tid)) } else { None };
                let tgid_identity = process_pid.identity();
                let tid_identity = thread_pid
                    .as_ref()
                    .map_or_else(|| tgid_identity.clone(), PreparedPid::identity);
                let thread_registration = thread_pid.as_mut().map_or_else(
                    || {
                        process_pid
                            .take_registration()
                            .clone_in(PidLinkSlot::try_new().unwrap())
                    },
                    PreparedPid::take_registration,
                );
                // 主线程分支的 temporary original link 不能被 take 后丢掉唯一 TGID registration。
                let process_registration = if tid == 11 {
                    thread_registration.clone_in(PidLinkSlot::try_new().unwrap())
                } else {
                    process_pid.take_registration()
                };
                let task = Arc::new(TaskControlBlock {
                    tgid: tgid_identity,
                    tid: spin::Mutex::new(tid_identity),
                    killed: false.into(),
                    retirements: super::super::model::initial_retirements(),
                });
                let mut graph = ProcessGraph {
                    nodes: FallibleMap::new(),
                    threads: FallibleMap::new(),
                };
                let mut members = FallibleMap::new();
                members.try_insert(tid, task.clone()).unwrap();
                graph
                    .nodes
                    .try_insert(
                        11,
                        ProcessNode {
                            pid: process_registration,
                            state: ProcessState::Live(members),
                            parent_thread: Some(1),
                            pdeath_cursor: 70,
                            exec_generation: 0,
                            exit_signal: 10,
                            group_exit: None,
                            exec: None,
                            job_control: JobControlState::Running,
                        },
                    )
                    .unwrap();
                let mut children = FallibleMap::new();
                children.try_insert(700, ()).unwrap();
                graph
                    .threads
                    .try_insert(
                        tid,
                        ThreadIndex {
                            pidfd_source: alloc::sync::Weak::new(),
                            pid: thread_registration,
                            tgid: 11,
                            created_children: children,
                        },
                    )
                    .unwrap();
                for id in [20, 200, 700] {
                    let mut pid = prepared(id);
                    let process_registration = pid.take_registration();
                    let thread_registration =
                        process_registration.clone_in(PidLinkSlot::try_new().unwrap());
                    graph
                        .nodes
                        .try_insert(
                            id,
                            ProcessNode {
                                pid: process_registration,
                                state: ProcessState::Zombie,
                                parent_thread: Some(if id == 700 { tid } else { 1 }),
                                pdeath_cursor: 0,
                                exec_generation: 0,
                                exit_signal: 10,
                                group_exit: None,
                                exec: None,
                                job_control: JobControlState::Running,
                            },
                        )
                        .unwrap();
                    graph
                        .threads
                        .try_insert(
                            id,
                            ThreadIndex {
                                pidfd_source: alloc::sync::Weak::new(),
                                pid: thread_registration,
                                tgid: id,
                                created_children: FallibleMap::new(),
                            },
                        )
                        .unwrap();
                }
                let retired = core::mem::replace(&mut *PROCESS_TABLE.graph.lock(), graph);
                drop(retired);
                task
            }
            fn add_sibling(tid: usize) -> Arc<TaskControlBlock> {
                let mut pid = prepared(tid);
                let (retirements, tgid_identity) = {
                    let graph = PROCESS_TABLE.graph.lock();
                    let ProcessState::Live(threads) = &graph.nodes.get(&11).unwrap().state else {
                        panic!("live");
                    };
                    let first = threads.first_key_value().unwrap().1;
                    (first.retirements.clone(), first.tgid.clone())
                };
                let sibling = Arc::new(TaskControlBlock {
                    tgid: tgid_identity,
                    tid: spin::Mutex::new(pid.identity()),
                    killed: false.into(),
                    retirements,
                });
                let member = FallibleMap::try_prepare(tid, sibling.clone()).unwrap();
                let index = FallibleMap::try_prepare(
                    tid,
                    ThreadIndex {
                        pidfd_source: alloc::sync::Weak::new(),
                        pid: pid.take_registration(),
                        tgid: 11,
                        created_children: FallibleMap::new(),
                    },
                )
                .unwrap();
                let mut graph = PROCESS_TABLE.graph.lock();
                let node = graph.nodes.get_mut(&11).unwrap();
                sibling.thread_retirements().publish_thread();
                let ProcessState::Live(threads) = &mut node.state else {
                    panic!("live");
                };
                threads.commit_vacant(member);
                graph.threads.commit_vacant(index);
                sibling
            }
            fn remove_sibling(
                tid: usize,
            ) -> super::super::model::thread_retirement::ThreadRetirement {
                let (retirement, member, index) = {
                    let mut graph = PROCESS_TABLE.graph.lock();
                    let node = graph.nodes.get_mut(&11).unwrap();
                    let ProcessState::Live(threads) = &mut node.state else {
                        panic!("live");
                    };
                    let member = threads.remove(&tid).unwrap();
                    let retirement = member.thread_retirements().retirement();
                    let index = graph.threads.remove(&tid).unwrap();
                    (retirement, member, index)
                };
                drop(member);
                drop(index);
                retirement
            }
            #[test]
            fn exec_preparation_failure_and_competing_claim_preserve_old_membership() {
                let _serial = SERIAL.lock().unwrap();
                let owner = setup(19);
                let sibling = add_sibling(30);
                crate::FAIL_NEXT.set(true);
                assert!(matches!(
                    exec_drain::ExecPreparation::claim(&owner),
                    Err(super::super::model::ExecError::Image(_))
                ));
                assert!(!crate::FAIL_NEXT.get());
                let claim = exec_drain::ExecPreparation::claim(&owner).unwrap();
                assert!(matches!(
                    exec_drain::ExecPreparation::claim(&sibling),
                    Err(super::super::model::ExecError::Busy)
                ));
                drop(claim);
                assert!(!sibling.killed.load(Ordering::Acquire));
                assert!(
                    PROCESS_TABLE
                        .graph
                        .lock()
                        .nodes
                        .get(&11)
                        .unwrap()
                        .exec
                        .is_none()
                );
                assert!(exec_drain::ExecPreparation::claim(&sibling).is_ok());
            }
            #[test]
            fn fatal_drain_abort_retires_claim_without_reusing_old_completion_identity() {
                let _serial = SERIAL.lock().unwrap();
                let owner = setup(19);
                let sibling = add_sibling(30);
                let target = crate::target::Target::new();
                let worker_target = target.clone();
                let worker_owner = owner.clone();
                let worker = std::thread::spawn(move || {
                    let _binding = worker_target.bind();
                    let claim = exec_drain::ExecPreparation::claim(&worker_owner).unwrap();
                    matches!(
                        claim.terminate_siblings(&worker_owner),
                        Err(super::super::model::ExecError::FatalInterrupted)
                    )
                });
                target.wait_until_sleeping();
                assert!(sibling.killed.load(Ordering::Acquire));
                let late_completion = remove_sibling(30);
                target.signal();
                target.fatal_signal();
                assert!(worker.join().unwrap());
                let next = exec_drain::ExecPreparation::claim(&owner).unwrap();
                late_completion.complete();
                next.terminate_siblings(&owner).unwrap();
                drop(next);
            }
            #[test]
            fn exec_also_waits_for_cleanup_removed_before_its_claim() {
                let _serial = SERIAL.lock().unwrap();
                let owner = setup(19);
                add_sibling(30);
                let old_retirement = remove_sibling(30);
                let target = crate::target::Target::new();
                let worker_target = target.clone();
                let worker_owner = owner.clone();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let _binding = worker_target.bind();
                    let claim = exec_drain::ExecPreparation::claim(&worker_owner).unwrap();
                    claim.terminate_siblings(&worker_owner).unwrap();
                    exec_identity::mark_process_exec(
                        &worker_owner,
                        Some(PidLinkSlot::try_new().unwrap()),
                    );
                    done_tx.send(()).unwrap();
                });
                target.wait_until_sleeping();
                assert_eq!(owner.tid(), 19);
                assert!(done_rx.try_recv().is_err());
                old_retirement.complete(); // Represents both old-MM and original VFORK completion.
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                worker.join().unwrap();
                assert_eq!(owner.tid(), 11);
            }
            #[test]
            fn de_thread_waits_for_retired_mm_cleanup_and_concurrent_thread_publication() {
                let _serial = SERIAL.lock().unwrap();
                let owner = setup(19);
                let sibling = add_sibling(30);
                let worker_owner = owner.clone();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let claim = exec_drain::ExecPreparation::claim(&worker_owner).unwrap();
                    claim.terminate_siblings(&worker_owner).unwrap();
                    exec_identity::mark_process_exec(
                        &worker_owner,
                        Some(PidLinkSlot::try_new().unwrap()),
                    );
                    drop(claim);
                    done_tx.send(()).unwrap();
                });
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !sibling.killed.load(Ordering::Acquire) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "drain did not publish sibling exit"
                    );
                    std::thread::yield_now();
                }
                // Creator still exists while late child is published: its token must extend drain.
                let late = add_sibling(31);
                let first = remove_sibling(30);
                let second = remove_sibling(31);
                assert!(matches!(
                    done_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                first.complete();
                assert!(matches!(
                    done_rx.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                second.complete();
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                worker.join().unwrap();
                assert_eq!(owner.tid(), 11);
                assert_eq!(late.tid(), 31);
                assert!(
                    PROCESS_TABLE
                        .graph
                        .lock()
                        .nodes
                        .get(&11)
                        .unwrap()
                        .exec
                        .is_none()
                );
            }
            #[test]
            fn nonleader_exec_rekeys_both_indexes_and_creator_relation_without_allocating() {
                let _serial = SERIAL.lock().unwrap();
                let task = setup(99);
                let old_source = Arc::new(crate::fs::pidfs::PidFdSource::default());
                PROCESS_TABLE
                    .graph
                    .lock()
                    .threads
                    .get_mut(&99)
                    .unwrap()
                    .pidfd_source = Arc::downgrade(&old_source);
                let link = if task.tid() != task.tgid() {
                    Some(PidLinkSlot::try_new().unwrap())
                } else {
                    None
                };
                crate::FAIL_NEXT.set(true);
                exec_identity::mark_process_exec(&task, link);
                assert!(crate::FAIL_NEXT.replace(false));
                assert_eq!(old_source.0.load(Ordering::Acquire), 1);
                assert!(
                    PROCESS_TABLE
                        .graph
                        .lock()
                        .threads
                        .get(&11)
                        .unwrap()
                        .pidfd_source
                        .upgrade()
                        .is_none()
                );
                assert_eq!(task.tid(), 11);
                let graph = PROCESS_TABLE.graph.lock();
                assert_eq!(graph.nodes.get(&11).unwrap().exec_generation, 1);
                assert_eq!(graph.nodes.get(&11).unwrap().exit_signal, 17);
                assert_eq!(graph.nodes.get(&11).unwrap().pdeath_cursor, 0);
                let ProcessState::Live(members) = &graph.nodes.get(&11).unwrap().state else {
                    panic!();
                };
                assert_eq!(members.len(), 1);
                assert!(members.get(&99).is_none());
                assert!(Arc::ptr_eq(members.get(&11).unwrap(), &task));
                assert!(graph.threads.get(&99).is_none());
                assert_eq!(
                    graph.threads.iter().map(|(&id, _)| id).collect::<Vec<_>>(),
                    [11, 20, 200, 700]
                );
                assert!(
                    graph
                        .threads
                        .get(&11)
                        .unwrap()
                        .created_children
                        .contains_key(&700)
                );
                assert_eq!(graph.nodes.get(&700).unwrap().parent_thread, Some(11));
            }
            #[test]
            fn original_leader_exec_preserves_index_and_pending_cursor() {
                let _serial = SERIAL.lock().unwrap();
                let task = setup(11);
                let link = if task.tid() != task.tgid() {
                    Some(PidLinkSlot::try_new().unwrap())
                } else {
                    None
                };
                crate::FAIL_NEXT.set(true);
                exec_identity::mark_process_exec(&task, link);
                assert!(crate::FAIL_NEXT.replace(false));
                let graph = PROCESS_TABLE.graph.lock();
                assert_eq!(task.tid(), 11);
                assert_eq!(graph.nodes.get(&11).unwrap().pdeath_cursor, 70);
                assert_eq!(graph.nodes.get(&11).unwrap().exec_generation, 1);
                assert_eq!(graph.nodes.get(&11).unwrap().exit_signal, 17);
                assert_eq!(graph.nodes.get(&700).unwrap().parent_thread, Some(11));
                assert!(
                    graph
                        .threads
                        .get(&11)
                        .unwrap()
                        .created_children
                        .contains_key(&700)
                );
            }
        }
    }
}
