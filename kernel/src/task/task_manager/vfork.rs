use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcessCloneError {
    Memory(crate::memory::MemoryError),
    ResourceLimit,
}

struct ChildGraphSlots {
    thread: crate::fallible_tree::NodeSlot<usize, Arc<TaskControlBlock>>,
    process: crate::fallible_tree::NodeSlot<usize, ProcessNode>,
    thread_index: crate::fallible_tree::NodeSlot<usize, ThreadIndex>,
    parent_child: crate::fallible_tree::NodeSlot<usize, ()>,
    creator_child: crate::fallible_tree::NodeSlot<usize, ()>,
    group_member: crate::fallible_tree::NodeSlot<usize, ()>,
    future_group: crate::fallible_tree::NodeSlot<(usize, usize), ProcessGroupIndex>,
}

impl ChildGraphSlots {
    fn try_new() -> Result<Self, ProcessCloneError> {
        let oom = |_| ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory);
        Ok(Self {
            thread: FallibleMap::try_reserve_node().map_err(oom)?,
            process: FallibleMap::try_reserve_node().map_err(oom)?,
            thread_index: FallibleMap::try_reserve_node().map_err(oom)?,
            parent_child: FallibleMap::try_reserve_node().map_err(oom)?,
            creator_child: FallibleMap::try_reserve_node().map_err(oom)?,
            group_member: FallibleMap::try_reserve_node().map_err(oom)?,
            future_group: FallibleMap::try_reserve_node().map_err(oom)?,
        })
    }
}

/// @description 把已完整准备的 fork/vfork child 一次发布到唯一 process graph。
///
/// @param parent parent TGID。
/// @param child 尚未进入 runqueue 的唯一 child Thread owner。
/// @param vfork_parent vfork 时被 child node 保活的 calling Thread；fork 为 None。
/// @return 无返回值；PID 重复或 parent 非 live 表示 graph 不变量损坏并 fail-stop。
fn publish_child(
    parent: usize,
    parent_thread: usize,
    child: Arc<TaskControlBlock>,
    vfork_parent: Option<Arc<TaskControlBlock>>,
    slots: ChildGraphSlots,
) {
    let pid = child.tgid();
    let mut graph = TASK_MANAGER.graph.lock();
    let parent_node = graph
        .nodes
        .get(&parent)
        .expect("fork parent disappeared before child publication");
    assert!(matches!(parent_node.state, ProcessState::Live(_)));
    let session = parent_node.session;
    let process_group = parent_node.process_group;
    let child_tid = child.tid();
    let mut threads = FallibleMap::new();
    threads.commit_vacant(slots.thread.fill(child_tid, child));
    graph.nodes.commit_vacant(slots.process.fill(
        pid,
        ProcessNode {
            parent: Some(parent),
            parent_thread: Some(parent_thread),
            children: FallibleMap::new(),
            session,
            process_group,
            group_slot: Some(slots.future_group),
            has_execed: false,
            state: ProcessState::Live(threads),
            group_exit: None,
            job_control: JobControlState::Running,
            exit_effects: 0,
            exit_effect_next: [None; 2],
            pdeath_enabled_threads: 0,
            pdeath_pending: false,
            pdeath_next: None,
            pdeath_cursor: 0,
            child_events: ChildEvents::default(),
            child_waiters: FallibleMap::new(),
            child_wait_claim: None,
            vfork_parent,
        },
    ));
    graph.threads.commit_vacant(slots.thread_index.fill(
        child_tid,
        ThreadIndex {
            tgid: pid,
            created_children: FallibleMap::new(),
        },
    ));
    graph
        .nodes
        .get_mut(&parent)
        .expect("fork parent disappeared during child publication")
        .children
        .commit_vacant(slots.parent_child.fill(pid, ()));
    graph
        .threads
        .get_mut(&parent_thread)
        .expect("fork creator thread disappeared during child publication")
        .created_children
        .commit_vacant(slots.creator_child.fill(pid, ()));
    graph
        .groups
        .get_mut(&(session, process_group))
        .expect("fork parent process group missing from graph")
        .members
        .commit_vacant(slots.group_member.fill(pid, ()));
    graph.processes_created = graph.processes_created.saturating_add(1);
}

/// @description COW fork 当前单线程 process 并发布 child 到唯一 graph/runqueue。
/// @return parent 成功获得 child PID；COW/page-table 事务 OOM 时 graph 不发布 child。
/// @errors 地址空间/Process 分配失败返回 Memory，RLIMIT_NPROC/PID namespace 耗尽返回 ResourceLimit。
pub(crate) fn fork_current_process() -> Result<usize, ProcessCloneError> {
    let parent = current_task().expect("fork requires current task");
    let pid = TASK_MANAGER
        .allocate_pid()
        .ok_or(ProcessCloneError::ResourceLimit)?;
    let graph_slots = ChildGraphSlots::try_new()?;
    let child = try_allocate_task(
        ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory),
        || parent.fork_process(pid).map_err(ProcessCloneError::Memory),
    )?;
    let child_pid = child.tgid();
    let mut minimum_snapshot_capacity = 0;
    let mut snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::Memory(error));
        }
    };
    loop {
        let creation = TASK_MANAGER.process_creation.lock();
        if let Err(required) = snapshot.capture() {
            drop(creation);
            minimum_snapshot_capacity = required;
            snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    child.remove_thread_trap_context();
                    return Err(ProcessCloneError::Memory(error));
                }
            };
            continue;
        }
        if !snapshot.allows_current() {
            drop(creation);
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::ResourceLimit);
        }
        publish_child(
            parent.tgid(),
            parent.tid(),
            child.clone(),
            None,
            graph_slots,
        );
        drop(creation);
        break;
    }
    drop(snapshot);
    enqueue_new_task(child);
    Ok(child_pid)
}

/// @description 发布共享 AddressSpace 的 vfork child，并只阻塞 calling Thread 到 child exec/exit。
/// @param child_stack musl clone wrapper 提供的 16-byte aligned child SP；零值继承。
/// @return parent 恢复后获得 child PID；准备失败时不发布 child 或 wait membership。
/// @errors 地址空间/Process 分配失败返回 Memory，RLIMIT_NPROC/PID namespace 耗尽返回 ResourceLimit。
pub(crate) fn vfork_current_process(child_stack: usize) -> Result<usize, ProcessCloneError> {
    let parent = current_task().expect("vfork requires current task");
    let pid = TASK_MANAGER
        .allocate_pid()
        .ok_or(ProcessCloneError::ResourceLimit)?;
    let graph_slots = ChildGraphSlots::try_new()?;
    let child = try_allocate_task(
        ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory),
        || {
            parent
                .vfork_process(pid, child_stack)
                .map_err(ProcessCloneError::Memory)
        },
    )?;
    let child_pid = child.tgid();
    let mut minimum_snapshot_capacity = 0;
    let mut snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::Memory(error));
        }
    };
    loop {
        let creation = TASK_MANAGER.process_creation.lock();
        if let Err(required) = snapshot.capture() {
            drop(creation);
            minimum_snapshot_capacity = required;
            snapshot = match ProcessSlotSnapshot::prepare(minimum_snapshot_capacity) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    child.remove_thread_trap_context();
                    return Err(ProcessCloneError::Memory(error));
                }
            };
            continue;
        }
        if !snapshot.allows_current() {
            drop(creation);
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::ResourceLimit);
        }
        publish_child(
            parent.tgid(),
            parent.tid(),
            child.clone(),
            Some(parent.clone()),
            graph_slots,
        );
        drop(creation);
        break;
    }
    drop(snapshot);

    let prepared = super::context_switch::prepare_current_block(&parent, (), |_, _| {
        WaitMembership::Vfork(child_pid)
    });
    enqueue_new_task(child);
    assert_eq!(
        prepared.suspend(),
        WaitResult::Woken,
        "vfork parent resumed without child exec/exit completion"
    );
    Ok(child_pid)
}

pub(super) fn complete_vfork(child_pid: usize) {
    let parent = TASK_MANAGER
        .graph
        .lock()
        .nodes
        .get_mut(&child_pid)
        .and_then(|node| node.vfork_parent.take());
    if let Some(parent) = parent {
        wake_parent(parent, child_pid);
    }
}

/// @description child 已切换到独立 AddressSpace 后完成 vfork exec handoff。
/// @param child_pid 已越过 exec point-of-no-return 的 child TGID。
/// @return 无返回值；非 vfork child 幂等忽略。
/// @errors 无错误。
pub(in crate::task) fn complete_vfork_exec(child_pid: usize) {
    complete_vfork(child_pid);
}

/// @description 消费 child exec/exit 发布的不可中断 vfork wait membership。
/// @param parent child node 移出的唯一 suspended parent。
/// @param child_pid 完成 exec/exit 的 vfork child TGID。
/// @return 无返回值；重复/stale completion 幂等忽略。
/// @errors 无错误。
pub(super) fn wake_parent(parent: Arc<TaskControlBlock>, child_pid: usize) {
    crate::task::processor::wake_waiting_task(
        parent,
        WaitMembership::Vfork(child_pid),
        Some(WaitResult::Woken),
    );
}
