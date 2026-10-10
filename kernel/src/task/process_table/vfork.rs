use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProcessCloneError {
    Memory(crate::memory::MemoryError),
    ResourceLimit,
    Pid(crate::namespace::PidAllocationError),
    PidFd(super::pidfd::PidFdError),
}

struct ChildGraphSlots {
    thread: crate::fallible_tree::NodeSlot<usize, Arc<TaskControlBlock>>,
    process: crate::fallible_tree::NodeSlot<usize, ProcessNode>,
    thread_index: crate::fallible_tree::NodeSlot<usize, ThreadIndex>,
    parent_child: crate::fallible_tree::NodeSlot<usize, ()>,
    creator_child: crate::fallible_tree::NodeSlot<usize, ()>,
    group_member: crate::fallible_tree::NodeSlot<usize, ()>,
    future_group: crate::fallible_tree::NodeSlot<usize, ProcessGroupIndex>,
    thread_pid: PidLinkSlot,
    namespace_exit: Option<Arc<super::pid_namespace::NamespaceExit>>,
    group_links: Option<(PidLinkSlot, PidLinkSlot)>,
}

impl ChildGraphSlots {
    fn try_new(namespace_init: bool) -> Result<Self, ProcessCloneError> {
        let oom = |_| ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory);
        Ok(Self {
            thread: FallibleMap::try_reserve_node().map_err(oom)?,
            process: FallibleMap::try_reserve_node().map_err(oom)?,
            thread_index: FallibleMap::try_reserve_node().map_err(oom)?,
            parent_child: FallibleMap::try_reserve_node().map_err(oom)?,
            creator_child: FallibleMap::try_reserve_node().map_err(oom)?,
            group_member: FallibleMap::try_reserve_node().map_err(oom)?,
            future_group: FallibleMap::try_reserve_node().map_err(oom)?,
            thread_pid: PidLinkSlot::try_new().map_err(ProcessCloneError::Pid)?,
            namespace_exit: if namespace_init {
                Some(
                    Arc::try_new(super::pid_namespace::NamespaceExit::new()).map_err(|_| {
                        ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory)
                    })?,
                )
            } else {
                None
            },
            group_links: if namespace_init {
                Some((
                    PidLinkSlot::try_new().map_err(ProcessCloneError::Pid)?,
                    PidLinkSlot::try_new().map_err(ProcessCloneError::Pid)?,
                ))
            } else {
                None
            },
        })
    }
}

/// 把已完整准备的 fork/vfork child 一次发布到唯一 process graph。
///
/// # Parameters
///
/// - `creator`: calling Thread；resource/security/terminal 始终从该实际执行体继承。
/// - `inherit_parent`: 最终 publication 继承 caller real-parent relation。
/// - `exit_signal`: 不继承 parent 时的 native exit notification。
/// - `child`: 尚未进入 runqueue 的唯一 child Thread owner。
/// - `vfork_parent`: vfork 时被 child Thread index 保活的 calling Thread；fork 为 None。
///
/// # Returns
///
/// 返回锁外释放的旧 security owner；PID 重复或 parent 非 live 表示 graph 不变量损坏。
#[allow(
    clippy::too_many_arguments,
    reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
)]
fn publish_child(
    creator: &TaskControlBlock,
    inherit_parent: bool,
    requested_exit_signal: u8,
    child: Arc<TaskControlBlock>,
    vfork_parent: Option<Arc<TaskControlBlock>>,
    slots: ChildGraphSlots,
    prepared_pid: &mut PreparedPid,
    pidfd_source: alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
) -> Result<super::super::model::RetiredSecurity, crate::namespace::PidAllocationError> {
    let pid = child.tgid();
    let mut graph = PROCESS_TABLE.graph.lock();
    let creator_node = graph
        .nodes
        .get(&creator.tgid())
        .expect("fork creator missing");
    let (parent, parent_thread, exit_signal, parent_exec_generation) = if inherit_parent {
        if creator.is_namespace_init() {
            return Err(crate::namespace::PidAllocationError::Invalid);
        }
        (
            creator_node
                .parent
                .expect("non-init creator requires real parent"),
            creator_node
                .parent_thread
                .expect("non-init creator requires real parent Thread"),
            creator_node.exit_signal,
            creator_node.parent_exec_generation,
        )
    } else {
        (
            creator.tgid(),
            creator.tid(),
            requested_exit_signal,
            creator_node.exec_generation,
        )
    };
    // relation 与 security 来源分开：CLONE_PARENT 的新 sibling 仍继承 caller，不继承 grandparent。
    let session = creator_node.session;
    let process_group = creator_node.process_group;
    prepared_pid.publish()?;
    let process_pid = prepared_pid.take_registration();
    let thread_pid = process_pid.clone_in(slots.thread_pid);
    let parent_node = graph
        .nodes
        .get(&parent)
        .expect("fork parent disappeared before child publication");
    let ProcessState::Live(parent_threads) = &parent_node.state else {
        panic!("fork parent is not live");
    };
    parent_threads
        .get(&parent_thread)
        .expect("fork real parent Thread is not live");
    let retired = child
        .security_owner()
        .inherit_from(creator.security_owner());
    assert!(
        child
            .process()
            .replace_terminal(creator.terminal())
            .is_none(),
        "unpublished child cannot already own a controlling TTY"
    );
    let namespace_init = process_pid.identity().is_namespace_init();
    let session = if namespace_init { pid } else { session };
    let process_group = if namespace_init { pid } else { process_group };
    let mut group_member = Some(slots.group_member);
    let mut group_slot = Some(slots.future_group);
    let new_group = if namespace_init {
        let (pgid, sid) = slots
            .group_links
            .expect("namespace init group links prepared");
        let pgid = process_pid.clone_in(pgid);
        let sid = process_pid.clone_in(sid);
        let mut members = FallibleMap::new();
        members.commit_vacant(
            group_member
                .take()
                .expect("child group member prepared")
                .fill(pid, ()),
        );
        Some(group_slot.take().expect("child group node prepared").fill(
            pid,
            ProcessGroupIndex {
                pgid,
                sid,
                members,
                exit_check_pending: false,
                exit_check_next: None,
                was_orphaned_stopped: false,
            },
        ))
    } else {
        None
    };
    let child_tid = child.tid();
    let leader_credentials = child.leader_credentials_handle();
    let process = child.process().clone();
    let mut threads = FallibleMap::new();
    threads.commit_vacant(slots.thread.fill(child_tid, child));
    graph.nodes.commit_vacant(slots.process.fill(
        pid,
        ProcessNode {
            process,
            pid: process_pid,
            pidfd_source,
            namespace_exit: slots.namespace_exit,
            leader_credentials,
            parent: Some(parent),
            parent_thread: Some(parent_thread),
            exit_signal,
            parent_exec_generation,
            children: FallibleMap::new(),
            session,
            process_group,
            group_slot,
            exec_generation: 0,
            state: ProcessState::Live(threads),
            group_exit: None,
            exec: None,
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
        },
    ));
    graph.threads.commit_vacant(slots.thread_index.fill(
        child_tid,
        ThreadIndex {
            pid: thread_pid,
            pidfd_source: alloc::sync::Weak::new(),
            tgid: pid,
            exit_in_progress: false,
            retiring_task: None,
            created_children: FallibleMap::new(),
            vfork_parent,
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
    if let Some(group) = new_group {
        graph.groups.commit_vacant(group);
    } else {
        graph
            .groups
            .get_mut(&process_group)
            .expect("fork parent process group missing from graph")
            .members
            .commit_vacant(
                group_member
                    .take()
                    .expect("child group member prepared")
                    .fill(pid, ()),
            );
    }
    graph.processes_created = graph.processes_created.saturating_add(1);
    Ok(retired)
}

/// 按 resource sharing bits 发布 Process；VFORK 仅决定 caller 的完成等待。
///
/// # Parameters
///
/// - `child_stack`: 零值继承 caller SP；非零值已由 syscall 验证 alignment。
/// - `resources`: syscall 已验证的资源共享与 VFORK 完成等待选择。
///
/// # Errors
///
/// preparation OOM 或 NPROC/PID 耗尽时不发布；所有组合共用唯一 graph transaction。
pub(crate) fn clone_current_process(
    parameters: crate::task::CloneParameters,
    resources: crate::task::ProcessCloneResources,
) -> Result<usize, ProcessCloneError> {
    let parent = current_task().expect("process clone requires current task");
    let mut pid = PROCESS_TABLE
        .allocate_pid(
            resources.namespaces.pid_for_children(),
            &parameters.requested_pids,
            &parent.namespace_authority(),
        )
        .map_err(ProcessCloneError::Pid)?;
    let graph_slots = ChildGraphSlots::try_new(pid.identity().is_namespace_init())?;
    let child = try_allocate_task(
        ProcessCloneError::Memory(crate::memory::MemoryError::OutOfMemory),
        || {
            parent
                .clone_process(
                    ProcessId::from_identity(pid.identity()),
                    &parameters,
                    &resources,
                )
                .map_err(ProcessCloneError::Memory)
        },
    )?;
    let wait_for_child = resources.wait_for_child;
    let inherit_parent = resources.inherit_parent;
    let exit_signal = resources.exit_signal;
    // 发布 child 前退休 preparation pins；VFORK wait 不能保活已被 child 替换的 namespace/FS path。
    drop(resources);
    let prepared_pidfd = match parameters
        .pidfd
        .map(|address| {
            super::pidfd::PreparedPidFd::for_clone(pid.identity().clone(), false, &parent, address)
        })
        .transpose()
    {
        Ok(prepared) => prepared,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::PidFd(error));
        }
    };
    let child_pid = child.tgid();
    let visible_pid = pid.identity().number_in(&parent.pid_namespace()) as usize;
    let mut minimum_snapshot_capacity = 0;
    let mut snapshot = match ThreadSlotSnapshot::prepare(minimum_snapshot_capacity) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            child.remove_thread_trap_context();
            return Err(ProcessCloneError::Memory(error));
        }
    };
    loop {
        let creation = PROCESS_TABLE.process_creation.lock();
        if let Err(required) = snapshot.capture() {
            drop(creation);
            minimum_snapshot_capacity = required;
            snapshot = match ThreadSlotSnapshot::prepare(minimum_snapshot_capacity) {
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
        let retired = publish_child(
            &parent,
            inherit_parent,
            exit_signal,
            child.clone(),
            wait_for_child.then(|| parent.clone()),
            graph_slots,
            &mut pid,
            prepared_pidfd
                .as_ref()
                .map_or_else(alloc::sync::Weak::new, |prepared| prepared.source()),
        );
        drop(creation);
        match retired {
            Ok(retired) => drop(retired),
            Err(error) => {
                child.remove_thread_trap_context();
                return Err(ProcessCloneError::Pid(error));
            }
        }
        break;
    }
    drop(snapshot);
    if let Some(prepared) = prepared_pidfd {
        prepared.publish();
    }

    // 两个地址分别属于 parent 与 child 的实际 MM；非 CLONE_VM 时不能写到 parent 的 child_tid。
    parent.write_clone_tid_values([parameters.parent_set_tid, None], visible_pid as i32);
    child.write_clone_tid_values([parameters.child_set_tid, None], child.visible_tid() as i32);
    drop(parameters);

    if wait_for_child {
        // 在 child 可运行前发布 caller 的 wait membership，防止 child completion 早到丢失。
        let prepared = super::context_switch::prepare_current_block(&parent, (), |_, _| {
            WaitMembership::Vfork(child_pid)
        });
        enqueue_new_task(child);
        assert_eq!(
            prepared.suspend(),
            WaitResult::Woken,
            "vfork parent resumed without child exec/exit completion"
        );
    } else {
        enqueue_new_task(child);
    }
    Ok(visible_pid)
}

#[path = "vfork/completion.rs"]
mod completion;
pub(in crate::task) use completion::complete_vfork_exec;
pub(super) use completion::take_vfork_completion;

/// 消费 child exec/exit 发布的不可中断 vfork wait membership。
///
/// # Parameters
///
/// - `parent`: child node 移出的唯一 suspended parent。
/// - `child_pid`: 完成 exec/exit 的 vfork child TGID。
///
/// # Returns
///
/// 无返回值；重复/stale completion 幂等忽略。
pub(super) fn wake_parent(parent: Arc<TaskControlBlock>, child_pid: usize) {
    crate::task::processor::wake_waiting_task(
        parent,
        WaitMembership::Vfork(child_pid),
        Some(WaitResult::Woken),
    );
}
