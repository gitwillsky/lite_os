//! 实际 graph installation 算法；宿主 graph/IRQ adapter 只提供 topology 与锁。
#![feature(allocator_ext)]
extern crate alloc;
#[allow(dead_code, unused_imports)]
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[allow(dead_code, unused_imports)]
#[path = "../src/namespace_fixture.rs"]
mod namespace;

#[allow(
    dead_code,
    reason = "TSYNC consumes killable locking; other TaskMutex modes are covered by synchronization fixtures"
)]
#[path = "../src/sync.rs"]
mod sync;
use alloc::sync::Arc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::{LazyLock, Mutex, MutexGuard},
};
use syscall_abi::seccomp::*;
macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
#[path = "seccomp/security.rs"]
mod model;
use model::seccomp::{SeccompDecision, SeccompError, ThreadSecurityOwner};
#[allow(dead_code, unused_imports)]
#[path = "../src/seccomp_environment.rs"]
mod seccomp_environment;
use seccomp_environment::{file, fs, id, ipc};
mod arch {
    pub(crate) mod user {
        pub(crate) const AUDIT_ARCH: u32 = 0xc000_00b7;
    }
}
mod task {
    pub(crate) fn current_task() -> Option<alloc::sync::Arc<crate::TaskControlBlock>> {
        unreachable!("descriptor notification processing belongs to seccomp_notifications")
    }
    pub(crate) use super::SeccompError;
    pub(crate) mod model {
        pub(crate) use crate::model::seccomp::{RetiredSecurity, ValidatedFilter};
    }
}
#[path = "../../../kernel/src/task/process_table/seccomp.rs"]
mod installation;

pub(crate) struct TaskControlBlock {
    pid: namespace::PidRegistration,
    security: ThreadSecurityOwner,
    credential_guard: Arc<sync::TaskMutex<()>>,
}
impl TaskControlBlock {
    fn credential_guard(&self) -> &sync::TaskMutex<()> {
        &self.credential_guard
    }
    fn thread(&self) -> &Self {
        self
    }
    fn tid(&self) -> usize {
        self.pid.identity().key()
    }
    fn tgid(&self) -> usize {
        2
    }
    fn tid_identity(&self) -> Arc<namespace::PidIdentity> {
        self.pid.identity().clone()
    }
    fn pid_namespace(&self) -> Arc<namespace::PidNamespace> {
        self.pid.identity().namespace()
    }
    fn user_program_counter(&self) -> usize {
        0x1234
    }
}
struct ThreadIndex {
    exit_in_progress: bool,
}
enum ProcessState {
    Live(BTreeMap<usize, Arc<TaskControlBlock>>),
    Exited(()),
}
struct ProcessNode {
    state: ProcessState,
    group_exit: Option<()>,
}
struct ProcessGraph {
    nodes: BTreeMap<usize, ProcessNode>,
    threads: BTreeMap<usize, ThreadIndex>,
}
struct ProcessTable {
    graph: GraphMutex,
}
struct GraphMutex(Mutex<ProcessGraph>);
struct GraphGuard<'a>(MutexGuard<'a, ProcessGraph>);
impl GraphMutex {
    fn lock(&self) -> GraphGuard<'_> {
        let hook = BEFORE_GRAPH.with_borrow_mut(Option::take);
        if let Some(hook) = hook {
            hook();
        }
        let guard = self.0.lock().unwrap();
        GRAPH_DEPTH.with(|depth| depth.set(depth.get() + 1));
        GraphGuard(guard)
    }
}
impl Deref for GraphGuard<'_> {
    type Target = ProcessGraph;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for GraphGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for GraphGuard<'_> {
    fn drop(&mut self) {
        GRAPH_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}
thread_local! {
    static GRAPH_DEPTH: Cell<usize> = const { Cell::new(0) };
    static LOCK_HEAP_OPERATIONS: Cell<usize> = const { Cell::new(0) };
    static ALLOCATION_COUNTDOWN: Cell<Option<usize>> = const { Cell::new(None) };
    static BEFORE_GRAPH: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<Arc<TaskControlBlock>>> = const { RefCell::new(None) };
}
struct Allocator;
fn heap_operation() {
    if GRAPH_DEPTH
        .try_with(|depth| depth.get() != 0)
        .unwrap_or(false)
    {
        let _ = LOCK_HEAP_OPERATIONS.try_with(|count| count.set(count.get() + 1));
    }
}
fn fail_allocation() -> bool {
    ALLOCATION_COUNTDOWN
        .try_with(|count| match count.get() {
            None => false,
            Some(0) => {
                count.set(None);
                true
            }
            Some(n) => {
                count.set(Some(n - 1));
                false
            }
        })
        .unwrap_or(false)
}
// SAFETY: 成功路径使用原样 System allocation；一次故障只返回 null，无伪造地址。
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        heap_operation();
        if fail_allocation() {
            core::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        heap_operation();
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        heap_operation();
        if fail_allocation() {
            core::ptr::null_mut()
        } else {
            unsafe { System.realloc(pointer, layout, size) }
        }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
static PROCESS_TABLE: LazyLock<ProcessTable> = LazyLock::new(|| table(1).0);
fn current_task() -> Option<Arc<TaskControlBlock>> {
    CURRENT.with_borrow(Clone::clone)
}
fn table(count: usize) -> (ProcessTable, Vec<Arc<TaskControlBlock>>) {
    let owner = namespace::boot();
    let view = namespace::initial_pid().child(owner, 0).unwrap();
    let credential_guard = Arc::new(sync::TaskMutex::new(()));
    let tasks: Vec<_> = (1..=count)
        .map(|tid| {
            let mut pid = namespace::PreparedPid::try_new(
                tid + 1,
                view.clone(),
                &[],
                &crate::namespace::root_authority(),
            )
            .unwrap();
            pid.publish().unwrap();
            Arc::new(TaskControlBlock {
                pid: pid.take_registration(),
                security: ThreadSecurityOwner::default(),
                credential_guard: credential_guard.clone(),
            })
        })
        .collect();
    let threads = tasks
        .iter()
        .map(|task| (task.tid(), task.clone()))
        .collect();
    let indices = tasks
        .iter()
        .map(|task| {
            (
                task.tid(),
                ThreadIndex {
                    exit_in_progress: false,
                },
            )
        })
        .collect();
    (
        ProcessTable {
            graph: GraphMutex(Mutex::new(ProcessGraph {
                nodes: BTreeMap::from([(
                    2,
                    ProcessNode {
                        state: ProcessState::Live(threads),
                        group_exit: None,
                    },
                )]),
                threads: indices,
            })),
        },
        tasks,
    )
}
fn constant(action: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[0] = 6;
    bytes[4..].copy_from_slice(&action.to_ne_bytes());
    bytes
}
fn decision(task: &TaskControlBlock) -> SeccompDecision {
    task.evaluate_seccomp(172, [0; 6])
}
impl ProcessTable {
    fn install_bytes(
        &self,
        current: &TaskControlBlock,
        bytes: &[u8],
        synchronize: bool,
    ) -> Result<(), SeccompError> {
        let program = model::seccomp::ValidatedFilter::prepare(bytes, 0)?;
        self.install_filter(current, &program, synchronize)
    }
}
fn leaf(table: &ProcessTable, task: &TaskControlBlock, action: u32) {
    table.install_bytes(task, &constant(action), false).unwrap();
}

#[test]
fn disabled_and_ancestor_threads_receive_one_policy_and_monotonic_nnp() {
    let (table, tasks) = table(3);
    leaf(&table, &tasks[0], RET_ERRNO | 1);
    drop(tasks[1].security.inherit_from(&tasks[0].security));
    leaf(&table, &tasks[0], RET_ERRNO | 2);
    tasks[0].security.enable_no_new_privileges();
    tasks[2].security.enable_no_new_privileges();
    LOCK_HEAP_OPERATIONS.set(0);
    table
        .install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true)
        .unwrap();
    assert_eq!(
        LOCK_HEAP_OPERATIONS.get(),
        0,
        "heap work inside graph commit"
    );
    for task in &tasks {
        assert_eq!(decision(task), SeccompDecision::Errno(13));
        assert!(task.security.no_new_privileges());
        assert_eq!(task.seccomp_mode(), MODE_FILTER);
    }
}

#[test]
fn identical_bytes_on_a_divergent_branch_are_not_ancestry_and_publish_nothing() {
    let (table, tasks) = table(3);
    leaf(&table, &tasks[0], RET_ERRNO | 1);
    drop(tasks[1].security.inherit_from(&tasks[0].security));
    leaf(&table, &tasks[0], RET_ALLOW);
    leaf(&table, &tasks[1], RET_ALLOW);
    assert_eq!(
        table.install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true),
        Err(SeccompError::Unsynchronized(2))
    );
    assert_eq!(decision(&tasks[0]), SeccompDecision::Errno(1));
    assert_eq!(tasks[2].seccomp_mode(), MODE_DISABLED);
    assert_eq!(decision(&tasks[1]), SeccompDecision::Errno(1));
}

#[test]
fn exiting_strict_thread_is_skipped_without_changing_live_membership() {
    let (table, tasks) = table(3);
    tasks[1].security.enable_strict().unwrap();
    assert_eq!(
        table.install_bytes(&tasks[0], &constant(RET_ALLOW), true),
        Err(SeccompError::Unsynchronized(2))
    );
    table
        .graph
        .lock()
        .threads
        .get_mut(&tasks[1].tid())
        .unwrap()
        .exit_in_progress = true;
    table
        .install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true)
        .unwrap();
    assert_eq!(tasks[1].seccomp_mode(), MODE_STRICT);
    assert_eq!(decision(&tasks[2]), SeccompDecision::Errno(13));
    assert_eq!(table.graph.lock().threads.len(), 3);
}

#[test]
fn group_exit_still_validates_but_only_publishes_the_caller() {
    let (table, tasks) = table(3);
    table.graph.lock().nodes.get_mut(&2).unwrap().group_exit = Some(());
    tasks[0].security.enable_no_new_privileges();
    tasks[1].security.enable_strict().unwrap();
    assert_eq!(
        table.install_bytes(&tasks[0], &constant(RET_ALLOW), true),
        Err(SeccompError::Unsynchronized(2))
    );
    table
        .graph
        .lock()
        .threads
        .get_mut(&tasks[1].tid())
        .unwrap()
        .exit_in_progress = true;
    table
        .install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true)
        .unwrap();
    assert_eq!(decision(&tasks[0]), SeccompDecision::Errno(13));
    assert_eq!(tasks[1].seccomp_mode(), MODE_STRICT);
    assert_eq!(tasks[2].seccomp_mode(), MODE_DISABLED);
    assert!(!tasks[2].security.no_new_privileges());
}

#[test]
fn remote_sync_between_prepare_and_commit_is_rebased_without_losing_the_new_leaf() {
    let (table, tasks) = table(2);
    let table = Arc::new(table);
    let remote_table = table.clone();
    let remote = tasks[1].clone();
    BEFORE_GRAPH.set(Some(Box::new(move || {
        remote_table
            .install_bytes(&remote, &constant(RET_ERRNO | 1), true)
            .unwrap();
    })));
    table
        .install_bytes(&tasks[0], &constant(RET_ERRNO | 13), false)
        .unwrap();
    assert_eq!(decision(&tasks[0]), SeccompDecision::Errno(13));
    assert_eq!(decision(&tasks[1]), SeccompDecision::Errno(1));
    // 再次 TSYNC 能证明远端原先的 leaf 确实保留为 ancestry，而非覆盖旧 policy。
    table
        .install_bytes(&tasks[0], &constant(RET_ALLOW), true)
        .unwrap();
    assert_eq!(decision(&tasks[1]), SeccompDecision::Errno(13));
}

#[test]
fn provisional_clone_snapshot_is_refreshed_at_publication() {
    let (table, tasks) = table(2);
    let child = tasks[0].security.clone();
    table
        .install_bytes(&tasks[1], &constant(RET_ERRNO | 13), true)
        .unwrap();
    tasks[0].security.enable_no_new_privileges();
    let retired = {
        let _graph = table.graph.lock();
        child.inherit_from(tasks[0].security_owner())
    };
    drop(retired);
    assert_eq!(
        child.evaluate(172, [0; 6], 0, || 0, |_| ()),
        SeccompDecision::Errno(13)
    );
    assert!(child.no_new_privileges());
}

#[test]
fn every_staging_allocation_failure_leaves_all_policies_unchanged() {
    let mut failures = 0;
    for budget in 0..10 {
        let (table, tasks) = table(3);
        leaf(&table, &tasks[0], RET_ERRNO | 1);
        drop(tasks[1].security.inherit_from(&tasks[0].security));
        ALLOCATION_COUNTDOWN.set(Some(budget));
        let result = table.install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true);
        ALLOCATION_COUNTDOWN.set(None);
        if result == Err(SeccompError::OutOfMemory) {
            failures += 1;
            assert_eq!(decision(&tasks[0]), SeccompDecision::Errno(1));
            assert_eq!(decision(&tasks[1]), SeccompDecision::Errno(1));
            assert_eq!(tasks[2].seccomp_mode(), MODE_DISABLED);
        } else {
            assert_eq!(result, Ok(()));
        }
    }
    assert!(
        failures >= 6,
        "all staging buffers/owners and capacity retry must be exercised"
    );
}

#[test]
fn concurrent_sync_and_append_keep_the_canonical_ancestor_prefix() {
    let (table, tasks) = table(2);
    let table = Arc::new(table);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let first_table = table.clone();
    let first = tasks[0].clone();
    let start = barrier.clone();
    let first_thread = std::thread::spawn(move || {
        start.wait();
        for _ in 0..100 {
            match first_table.install_bytes(&first, &constant(RET_ERRNO | 1), true) {
                Ok(()) => (),
                Err(SeccompError::Unsynchronized(2)) => return,
                other => panic!("unexpected sync result: {other:?}"),
            }
        }
    });
    let second_table = table.clone();
    let second = tasks[1].clone();
    let second_thread = std::thread::spawn(move || {
        barrier.wait();
        for _ in 0..100 {
            match second_table.install_bytes(&second, &constant(RET_ERRNO | 13), false) {
                Ok(()) => (),
                error => panic!("{error:?}"),
            }
        }
    });
    // sibling 独立追加后失去 ancestry，TSYNC 必须返回其 TID，不能静默覆盖新分支。
    first_thread.join().unwrap();
    second_thread.join().unwrap();
    assert_eq!(decision(&tasks[1]), SeccompDecision::Errno(13));
    assert!(matches!(
        decision(&tasks[0]),
        SeccompDecision::Allow | SeccompDecision::Errno(1)
    ));
}

#[test]
fn concurrent_target_nnp_is_preserved_when_caller_did_not_set_it() {
    let (table, tasks) = table(2);
    let target = tasks[1].clone();
    BEFORE_GRAPH.set(Some(Box::new(move || {
        target.security.enable_no_new_privileges()
    })));
    table
        .install_bytes(&tasks[0], &constant(RET_ALLOW), true)
        .unwrap();
    assert!(!tasks[0].security.no_new_privileges());
    assert!(tasks[1].security.no_new_privileges());
}

#[test]
fn thread_growth_after_capacity_preparation_retries_before_any_publication() {
    let (table, tasks) = table(2);
    let table = Arc::new(table);
    let mut pid = namespace::PreparedPid::try_new(
        4,
        tasks[0].pid_namespace(),
        &[],
        &crate::namespace::root_authority(),
    )
    .unwrap();
    pid.publish().unwrap();
    let newcomer = Arc::new(TaskControlBlock {
        pid: pid.take_registration(),
        security: ThreadSecurityOwner::default(),
        credential_guard: tasks[0].credential_guard.clone(),
    });
    let published = newcomer.clone();
    let destination = table.clone();
    BEFORE_GRAPH.set(Some(Box::new(move || {
        // 第一次 capture 知道 2 targets；第二次准备完容量后、commit 前才加入第三个。
        BEFORE_GRAPH.set(Some(Box::new(move || {
            let mut graph = destination.graph.lock();
            let ProcessState::Live(threads) = &mut graph.nodes.get_mut(&2).unwrap().state else {
                unreachable!()
            };
            let key = published.tid();
            threads.insert(key, published);
            graph.threads.insert(
                key,
                ThreadIndex {
                    exit_in_progress: false,
                },
            );
        })));
    })));
    table
        .install_bytes(&tasks[0], &constant(RET_ERRNO | 13), true)
        .unwrap();
    assert_eq!(decision(&newcomer), SeccompDecision::Errno(13));
    for task in &tasks {
        assert_eq!(decision(task), SeccompDecision::Errno(13));
    }
}

#[test]
#[ignore = "CPU diagnostic; actual production prepare/commit owner, not a product gate"]
fn production_tsync_cost() {
    use std::{hint::black_box, time::Instant};
    for thread_count in [1, 64] {
        let mut samples = Vec::new();
        for _ in 0..5 {
            let (table, tasks) = table(thread_count);
            for _ in 0..128 {
                table
                    .install_bytes(&tasks[0], &constant(RET_ALLOW), true)
                    .unwrap();
            }
            let start = Instant::now();
            for _ in 0..256 {
                table
                    .install_bytes(&tasks[0], black_box(&constant(RET_ALLOW)), true)
                    .unwrap();
            }
            samples.push(start.elapsed().as_nanos() as f64 / 256.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "seccomp TSYNC threads={thread_count} chain=128..384 median={:.2} ns/install",
            samples[2]
        );
    }
}

#[test]
fn facade_strict_installation_uses_the_same_owner() {
    let task = {
        let graph = PROCESS_TABLE.graph.lock();
        let ProcessState::Live(threads) = &graph.nodes.get(&2).unwrap().state else {
            unreachable!()
        };
        threads.get(&2).unwrap().clone()
    };
    CURRENT.set(Some(task.clone()));
    installation::install_seccomp_filter(&constant(RET_ALLOW), 0).unwrap();
    assert_eq!(
        installation::enable_strict_seccomp(),
        Err(SeccompError::Invalid)
    );
    CURRENT.set(None);
    assert!(matches!(ProcessState::Exited(()), ProcessState::Exited(())));
}
