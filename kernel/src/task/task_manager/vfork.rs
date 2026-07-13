use super::*;

/// @description 把已完整准备的 fork/vfork child 一次发布到唯一 process graph。
///
/// @param parent parent TGID。
/// @param child 尚未进入 runqueue 的唯一 child Thread owner。
/// @param vfork_parent vfork 时被 child node 保活的 calling Thread；fork 为 None。
/// @return 无返回值；PID 重复或 parent 非 live 表示 graph 不变量损坏并 fail-stop。
fn publish_child(
    parent: usize,
    child: Arc<TaskControlBlock>,
    vfork_parent: Option<Arc<TaskControlBlock>>,
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
    let mut threads = BTreeMap::new();
    threads.insert(child.tid(), child);
    let previous = graph.nodes.insert(
        pid,
        ProcessNode {
            parent: Some(parent),
            session,
            process_group,
            has_execed: false,
            state: ProcessState::Live(threads),
            group_exit: None,
            job_control: JobControlState::Running,
            child_events: ChildEvents::default(),
            child_waiters: BTreeMap::new(),
            child_wait_claim: None,
            vfork_parent,
        },
    );
    assert!(previous.is_none(), "allocated PID already exists");
    graph.processes_created = graph.processes_created.saturating_add(1);
}

/// @description COW fork 当前单线程 process 并发布 child 到唯一 graph/runqueue。
/// @return parent 成功获得 child PID；COW/page-table 事务 OOM 时 graph 不发布 child。
/// @errors 地址空间或 Process 资源分配失败时返回 MemoryError。
pub(crate) fn fork_current_process() -> Result<usize, crate::memory::MemoryError> {
    let parent = current_task().expect("fork requires current task");
    let pid = TASK_MANAGER.allocate_pid();
    let child = Arc::new(parent.fork_process(pid)?);
    let child_pid = child.tgid();
    publish_child(parent.tgid(), child.clone(), None);
    enqueue_new_task(child);
    Ok(child_pid)
}

/// @description 发布共享 AddressSpace 的 vfork child，并只阻塞 calling Thread 到 child exec/exit。
/// @param child_stack musl clone wrapper 提供的 16-byte aligned child SP；零值继承。
/// @return parent 恢复后获得 child PID；准备失败时不发布 child 或 wait membership。
/// @errors 地址空间或 Process 资源分配失败时返回 MemoryError。
pub(crate) fn vfork_current_process(
    child_stack: usize,
) -> Result<usize, crate::memory::MemoryError> {
    let parent = current_task().expect("vfork requires current task");
    let pid = TASK_MANAGER.allocate_pid();
    let child = Arc::new(parent.vfork_process(pid, child_stack)?);
    let child_pid = child.tgid();
    publish_child(parent.tgid(), child.clone(), Some(parent.clone()));

    let cpu = hart_id();
    let end_time = get_time_us();
    let mut sched = parent.scheduling.policy.lock();
    let runtime = end_time.saturating_sub(sched.last_runtime);
    sched.update_vruntime(runtime);
    drop(sched);
    with_current_processor(|processor| {
        let current = processor
            .take_current()
            .expect("vfork wait requires current task");
        assert!(Arc::ptr_eq(&current, &parent));
        let mut scheduling = parent.scheduling.state.lock();
        assert_eq!(scheduling.run_state, RunState::Running { cpu });
        assert!(scheduling.wait.is_none());
        assert!(scheduling.wait_result.is_none());
        scheduling.wait = Some(WaitMembership::Vfork(child_pid));
        scheduling.run_state = RunState::Blocking { cpu };
    });
    enqueue_new_task(child);
    schedule_with_task_context(parent.clone());
    assert_eq!(
        parent.scheduling.state.lock().wait_result.take(),
        Some(WaitResult::Woken),
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
