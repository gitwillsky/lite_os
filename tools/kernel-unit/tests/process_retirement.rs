#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production graph retirement with actual FallibleMap membership storage. Process lifecycle
//! admission and credential payload are explicit host seams; deferred consequence routing is real.
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
use std::cell::Cell;
thread_local! { static GRAPH_HELD: Cell<bool> = const { Cell::new(false) }; }
mod sync {
    pub(crate) enum WaitResult {
        Woken,
    }
}
mod fs {
    pub(crate) mod pidfs {
        #[derive(Default)]
        pub(crate) struct PidFdSource(pub(crate) core::sync::atomic::AtomicUsize);
        impl PidFdSource {
            pub(crate) fn record_exit(&self, _: i32) {
                assert!(!crate::GRAPH_HELD.with(|held| held.get()));
            }
            pub(crate) fn notify(&self) {
                assert!(!crate::GRAPH_HELD.with(|held| held.get()));
                self.0.fetch_add(1, core::sync::atomic::Ordering::Release);
            }
        }
    }
}
#[path = "../../../kernel/src/task"]
mod task {
    pub(crate) mod pid;
    pub(crate) mod processor {
        pub(in crate::task) fn wake_child_task(
            _: alloc::sync::Arc<super::process_table::TaskControlBlock>,
            _: crate::sync::WaitResult,
        ) {
            assert!(!crate::GRAPH_HELD.with(|held| held.get()));
        }
    }
    pub(crate) mod process_table {
        use crate::fallible_tree::FallibleMap;
        use alloc::sync::Arc;
        mod group_retirement;
        mod process_retirement;
        use process_retirement::RetiredProcesses;
        pub(in crate::task) struct TaskControlBlock;
        #[derive(Clone, Copy)]
        struct ProcessExitStatus(i32);
        impl ProcessExitStatus {
            fn wait_status(self) -> i32 {
                self.0
            }
        }
        enum ProcessState {
            Live(FallibleMap<usize, Arc<TaskControlBlock>>),
            Exited(ProcessExitStatus, ExitRetirement),
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum ExitRetirement {
            Retiring,
            Waitable,
            AfterClaim,
        }
        struct ThreadIndex {
            created_children: FallibleMap<usize, ()>,
            tgid: usize,
            vfork_parent: Option<Arc<TaskControlBlock>>,
        }
        struct ProcessGroupIndex {
            pgid: super::pid::PidRegistration,
            sid: super::pid::PidRegistration,
            exit_check_next: Option<usize>,
            members: FallibleMap<usize, ()>,
        }
        struct ProcessNode {
            pidfd_source: alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
            group_slot: Option<crate::fallible_tree::NodeSlot<usize, ProcessGroupIndex>>,
            credentials: Arc<()>,
            parent: Option<usize>,
            parent_thread: Option<usize>,
            session: usize,
            process_group: usize,
            state: ProcessState,
            children: FallibleMap<usize, ()>,
            child_waiters: FallibleMap<usize, Arc<TaskControlBlock>>,
            child_wait_claim: Option<wait_child::ChildWaitClaim>,
            child_events: ChildEvents,
            pdeath_next: Option<usize>,
            exit_effect_next: [Option<usize>; 2],
        }
        impl Drop for ProcessNode {
            fn drop(&mut self) {
                assert!(
                    !crate::GRAPH_HELD.with(CellExt::held),
                    "Process resource destroyed under graph"
                );
            }
        }
        trait CellExt {
            fn held(&self) -> bool;
        }
        impl CellExt for std::cell::Cell<bool> {
            fn held(&self) -> bool {
                self.get()
            }
        }
        struct ProcessGraph {
            exit_group_head: Option<usize>,
            nodes: FallibleMap<usize, ProcessNode>,
            groups: FallibleMap<usize, ProcessGroupIndex>,
            threads: FallibleMap<usize, ThreadIndex>,
            pdeath_head: Option<usize>,
            exit_effect_heads: [Option<usize>; 2],
        }
        fn node(parent: Option<usize>) -> ProcessNode {
            ProcessNode {
                pidfd_source: alloc::sync::Weak::new(),
                group_slot: None,
                credentials: Arc::new(()),
                parent,
                parent_thread: parent,
                session: 1,
                process_group: 1,
                state: ProcessState::Exited(ProcessExitStatus(0), ExitRetirement::Waitable),
                children: FallibleMap::new(),
                child_waiters: FallibleMap::new(),
                child_wait_claim: None,
                child_events: ChildEvents::default(),
                pdeath_next: None,
                exit_effect_next: [None; 2],
            }
        }
        #[derive(Default)]
        struct ChildEvents {
            stopped: Option<usize>,
            continued: bool,
        }
        struct GraphMutex(spin::Mutex<ProcessGraph>);
        struct GraphGuard<'a>(spin::MutexGuard<'a, ProcessGraph>);
        impl GraphMutex {
            fn lock(&self) -> GraphGuard<'_> {
                let guard = self.0.lock();
                crate::GRAPH_HELD.with(|held| held.set(true));
                GraphGuard(guard)
            }
        }
        impl core::ops::Deref for GraphGuard<'_> {
            type Target = ProcessGraph;
            fn deref(&self) -> &ProcessGraph {
                &self.0
            }
        }
        impl core::ops::DerefMut for GraphGuard<'_> {
            fn deref_mut(&mut self) -> &mut ProcessGraph {
                &mut self.0
            }
        }
        impl Drop for GraphGuard<'_> {
            fn drop(&mut self) {
                crate::GRAPH_HELD.with(|held| held.set(false));
            }
        }
        struct ProcessTable {
            graph: GraphMutex,
        }
        static PROCESS_TABLE: ProcessTable = ProcessTable {
            graph: GraphMutex(spin::Mutex::new(ProcessGraph {
                exit_group_head: None,
                nodes: FallibleMap::new(),
                groups: FallibleMap::new(),
                threads: FallibleMap::new(),
                pdeath_head: None,
                exit_effect_heads: [None; 2],
            })),
        };
        static CASE_LOCK: spin::Mutex<()> = spin::Mutex::new(());
        fn take_child_waiters(node: &mut ProcessNode) -> FallibleMap<usize, Arc<TaskControlBlock>> {
            core::mem::take(&mut node.child_waiters)
        }
        mod ptrace {
            pub(super) fn finish_claim(_: super::wait_child::ChildExit, _: bool) {
                unreachable!("retirement fixture has no tracer relation");
            }
        }
        mod wait_child {
            use super::*;
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(in crate::task::process_table) enum ChildStatusKind {
                Exited,
                Stopped,
                Continued,
                TracedStop,
                TracedExit,
            }
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(in crate::task::process_table) struct ChildWaitClaim {
                pub(in crate::task::process_table) waiter: usize,
                pub(in crate::task::process_table) kind: ChildStatusKind,
            }
            pub(crate) struct ChildExit {
                pub(super) pid: usize,
                pub(super) kind: ChildStatusKind,
                pub(super) claimant: usize,
            }
            // 真实实现把 child 的使用量并入 parent；夹具的图节点没有 Process，退休语义不依赖它。
            fn absorb_reaped_usage(_: &ProcessGraph, _: usize, _: Option<usize>) {}
            #[path = "claim.rs"]
            mod claim;
            pub(crate) use claim::{consume_child_status, release_child_status};
        }
        mod vfork {
            use super::*;
            thread_local! { static WOKEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
            fn wake_parent(_: Arc<TaskControlBlock>, child: usize) {
                assert!(!crate::GRAPH_HELD.with(|held| held.get()));
                WOKEN.with(|woken| woken.set(child));
            }
            mod completion;
            pub(super) use completion::take_vfork_completion;
            pub(super) fn woken() -> usize {
                WOKEN.with(|woken| woken.get())
            }
        }
        use crate::sync::WaitResult;

        #[test]
        fn actual_reap_notifies_pidfd_outside_graph_and_never_pins_the_process() {
            let source = Arc::new(crate::fs::pidfs::PidFdSource::default());
            let mut graph = fixture();
            graph.nodes.get_mut(&3).unwrap().pidfd_source = Arc::downgrade(&source);
            let mut retired = RetiredProcesses::default();
            crate::GRAPH_HELD.with(|held| held.set(true));
            retired.retire(&mut graph, 3);
            assert_eq!(source.0.load(core::sync::atomic::Ordering::Acquire), 0);
            assert!(!graph.nodes.contains_key(&3));
            crate::GRAPH_HELD.with(|held| held.set(false));
            drop(retired);
            assert_eq!(source.0.load(core::sync::atomic::Ordering::Acquire), 1);
            assert_eq!(Arc::strong_count(&source), 1);
        }

        fn fixture() -> ProcessGraph {
            let mut graph = ProcessGraph {
                exit_group_head: None,
                nodes: FallibleMap::new(),
                groups: FallibleMap::new(),
                threads: FallibleMap::new(),
                pdeath_head: Some(2),
                exit_effect_heads: [Some(3), Some(4)],
            };
            graph.nodes.try_insert(1, node(None)).unwrap();
            graph
                .groups
                .try_insert(1, {
                    crate::namespace::boot();
                    let mut pid: super::pid::PreparedPid = crate::namespace::prepared_pid();
                    let pgid = pid.take_registration();
                    let sid = pgid.clone_in(super::pid::PidLinkSlot::try_new().unwrap());
                    ProcessGroupIndex {
                        pgid,
                        sid,
                        exit_check_next: None,
                        members: FallibleMap::new(),
                    }
                })
                .unwrap();
            graph
                .threads
                .try_insert(
                    1,
                    ThreadIndex {
                        created_children: FallibleMap::new(),
                        tgid: 1,
                        vfork_parent: None,
                    },
                )
                .unwrap();
            for pid in 2..=4 {
                graph.nodes.try_insert(pid, node(Some(1))).unwrap();
                graph
                    .nodes
                    .get_mut(&1)
                    .unwrap()
                    .children
                    .try_insert(pid, ())
                    .unwrap();
                graph
                    .threads
                    .get_mut(&1)
                    .unwrap()
                    .created_children
                    .try_insert(pid, ())
                    .unwrap();
                graph
                    .groups
                    .get_mut(&1)
                    .unwrap()
                    .members
                    .try_insert(pid, ())
                    .unwrap();
            }
            graph.nodes.get_mut(&2).unwrap().pdeath_next = Some(3);
            graph.nodes.get_mut(&3).unwrap().pdeath_next = Some(4);
            graph.nodes.get_mut(&3).unwrap().exit_effect_next[0] = Some(2);
            graph.nodes.get_mut(&2).unwrap().exit_effect_next[0] = Some(4);
            graph.nodes.get_mut(&4).unwrap().exit_effect_next[1] = Some(3);
            graph.nodes.get_mut(&3).unwrap().exit_effect_next[1] = Some(2);
            graph
        }
        #[test]
        fn retirement_detaches_all_indexes_and_queue_positions_before_resource_drop() {
            let mut graph = fixture();
            let observer = Arc::downgrade(&graph.nodes.get(&3).unwrap().credentials);
            let mut retired = RetiredProcesses::default();
            crate::GRAPH_HELD.with(|held| held.set(true));
            retired.retire(&mut graph, 3);
            assert!(!graph.nodes.contains_key(&3));
            assert!(!graph.nodes.get(&1).unwrap().children.contains_key(&3));
            assert!(
                !graph
                    .threads
                    .get(&1)
                    .unwrap()
                    .created_children
                    .contains_key(&3)
            );
            assert!(!graph.groups.get(&1).unwrap().members.contains_key(&3));
            assert_eq!(graph.pdeath_head, Some(2));
            assert_eq!(graph.nodes.get(&2).unwrap().pdeath_next, Some(4));
            assert_eq!(graph.exit_effect_heads, [Some(2), Some(4)]);
            assert_eq!(graph.nodes.get(&4).unwrap().exit_effect_next[1], Some(2));
            assert!(observer.upgrade().is_some());
            retired.retire(&mut graph, 2);
            retired.retire(&mut graph, 4);
            assert_eq!(graph.pdeath_head, None);
            assert_eq!(graph.exit_effect_heads, [None; 2]);
            assert!(graph.nodes.get(&1).unwrap().children.is_empty());
            crate::GRAPH_HELD.with(|held| held.set(false));
            drop(retired);
            assert!(observer.upgrade().is_none());
        }
        #[test]
        fn vanished_creator_index_is_not_recreated_during_reap() {
            let mut graph = fixture();
            graph.threads.remove(&1);
            let mut retired = RetiredProcesses::default();
            retired.retire(&mut graph, 2);
            assert!(graph.threads.is_empty());
        }
        #[test]
        fn active_job_claim_completes_committed_autoreap_on_success_and_copyout_failure() {
            use wait_child::{ChildExit, ChildStatusKind, ChildWaitClaim};
            let _case = CASE_LOCK.lock();
            for kind in [ChildStatusKind::Stopped, ChildStatusKind::Continued] {
                for copyout_success in [false, true] {
                    let mut graph = fixture();
                    let node = graph.nodes.get_mut(&3).unwrap();
                    node.state =
                        ProcessState::Exited(ProcessExitStatus(0), ExitRetirement::AfterClaim);
                    node.child_wait_claim = Some(ChildWaitClaim { waiter: 10, kind });
                    node.child_events.stopped = Some(19);
                    node.child_events.continued = true;
                    graph
                        .nodes
                        .get_mut(&1)
                        .unwrap()
                        .child_waiters
                        .try_insert(10, Arc::new(TaskControlBlock))
                        .unwrap();
                    let credentials = Arc::downgrade(&graph.nodes.get(&3).unwrap().credentials);
                    let old = {
                        let mut guard = PROCESS_TABLE.graph.lock();
                        core::mem::replace(&mut *guard, graph)
                    };
                    drop(old);
                    let record = ChildExit {
                        pid: 3,
                        kind,
                        claimant: 10,
                    };
                    if copyout_success {
                        wait_child::consume_child_status(record);
                    } else {
                        wait_child::release_child_status(record);
                    }
                    assert!(credentials.upgrade().is_none());
                    let guard = PROCESS_TABLE.graph.lock();
                    assert!(!guard.nodes.contains_key(&3));
                    assert!(guard.nodes.get(&1).unwrap().child_waiters.is_empty());
                    assert_eq!(guard.nodes.get(&2).unwrap().pdeath_next, Some(4));
                }
            }
        }
        #[test]
        fn released_observation_claim_preserves_waitable_exit_without_rechecking_disposition() {
            use wait_child::{ChildExit, ChildStatusKind, ChildWaitClaim};
            let _case = CASE_LOCK.lock();
            let mut graph = fixture();
            graph.nodes.get_mut(&3).unwrap().child_wait_claim = Some(ChildWaitClaim {
                waiter: 10,
                kind: ChildStatusKind::Exited,
            });
            let old = {
                let mut guard = PROCESS_TABLE.graph.lock();
                core::mem::replace(&mut *guard, graph)
            };
            drop(old);
            wait_child::release_child_status(ChildExit {
                pid: 3,
                kind: ChildStatusKind::Exited,
                claimant: 10,
            });
            let guard = PROCESS_TABLE.graph.lock();
            assert!(matches!(
                guard.nodes.get(&3).unwrap().state,
                ProcessState::Exited(_, ExitRetirement::Waitable)
            ));
            assert!(guard.nodes.get(&3).unwrap().child_wait_claim.is_none());
        }
        #[test]
        fn vfork_completion_survives_autoreap_and_belongs_only_to_original_child_thread() {
            let mut graph = fixture();
            graph
                .threads
                .try_insert(
                    3,
                    ThreadIndex {
                        tgid: 3,
                        created_children: FallibleMap::new(),
                        vfork_parent: Some(Arc::new(TaskControlBlock)),
                    },
                )
                .unwrap();
            graph
                .threads
                .try_insert(
                    30,
                    ThreadIndex {
                        tgid: 3,
                        created_children: FallibleMap::new(),
                        vfork_parent: None,
                    },
                )
                .unwrap();
            assert!(vfork::take_vfork_completion(&mut graph, 30).is_none());
            let completion = vfork::take_vfork_completion(&mut graph, 3).unwrap();
            assert!(vfork::take_vfork_completion(&mut graph, 3).is_none());
            graph.threads.remove(&3);
            let mut retired = RetiredProcesses::default();
            crate::GRAPH_HELD.with(|held| held.set(true));
            retired.retire(&mut graph, 3);
            crate::GRAPH_HELD.with(|held| held.set(false));
            drop(retired);
            assert!(!graph.nodes.contains_key(&3));
            completion.complete();
            assert_eq!(vfork::woken(), 3);
        }
    }
}
