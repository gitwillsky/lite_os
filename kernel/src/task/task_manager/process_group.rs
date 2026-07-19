use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessGroupError {
    NotFound,
    Permission,
    NotTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetProcessGroupError {
    NotFound,
    Permission,
    Executed,
}

/// @description 将当前 Process 建为新 session 与 process-group leader。
///
/// @return 成功返回新 SID（等于 TGID）。
/// @errors 当前 PID 已是任一 process group ID 时返回 Permission。
pub(crate) fn create_session() -> Result<usize, ProcessGroupError> {
    let pid = current_task().expect("setsid requires current task").tgid();
    let mut graph = TASK_MANAGER.graph.lock();
    if graph
        .groups
        .iter()
        .any(|((_, pgid), group)| *pgid == pid && !group.members.is_empty())
    {
        return Err(ProcessGroupError::Permission);
    }
    let old_group = {
        let node = graph
            .nodes
            .get_mut(&pid)
            .ok_or(ProcessGroupError::NotFound)?;
        (node.session, node.process_group)
    };
    let member = graph
        .groups
        .get_mut(&old_group)
        .expect("setsid old group missing")
        .members
        .take_entry(&pid)
        .expect("setsid member missing from old group");
    let new_group = (pid, pid);
    if let Some(group) = graph.groups.get_mut(&new_group) {
        debug_assert!(group.members.is_empty());
        group.members.commit_vacant(member);
    } else {
        let group_slot = graph
            .nodes
            .get_mut(&pid)
            .expect("setsid process disappeared under graph lock")
            .group_slot
            .take()
            .expect("setsid group node not reserved");
        let mut members = FallibleMap::new();
        members.commit_vacant(member);
        graph.groups.commit_vacant(group_slot.fill(
            new_group,
            ProcessGroupIndex {
                members,
                exit_check_pending: false,
                exit_check_next: None,
                was_orphaned_stopped: false,
            },
        ));
    }
    let node = graph
        .nodes
        .get_mut(&pid)
        .expect("setsid process disappeared under graph lock");
    node.session = pid;
    node.process_group = pid;
    Ok(pid)
}

/// @description 查询指定 Process 的 process group ID。
///
/// @param pid 零表示当前 TGID，否则为目标 TGID。
/// @return live/zombie process 的 PGID。
/// @errors 目标不存在时返回 NotFound。
pub(crate) fn process_group(pid: usize) -> Result<usize, ProcessGroupError> {
    let current = current_task()
        .expect("getpgid requires current task")
        .tgid();
    TASK_MANAGER
        .graph
        .lock()
        .nodes
        .get(&if pid == 0 { current } else { pid })
        .map(|node| node.process_group)
        .ok_or(ProcessGroupError::NotFound)
}

/// @description 查询指定 Process 的 session ID。
///
/// @param pid 零表示当前 TGID，否则为目标 TGID。
/// @return live/zombie process 的 SID。
/// @errors 目标不存在时返回 NotFound。
pub(crate) fn session_id(pid: usize) -> Result<usize, ProcessGroupError> {
    let current = current_task().expect("getsid requires current task").tgid();
    TASK_MANAGER
        .graph
        .lock()
        .nodes
        .get(&if pid == 0 { current } else { pid })
        .map(|node| node.session)
        .ok_or(ProcessGroupError::NotFound)
}

/// @description 按 Linux parent/child/session/exec 约束修改 process group membership。
///
/// @param pid 零表示 caller；非零只允许 caller 的直接 child。
/// @param pgid 零表示目标 TGID；非零必须是同 session 已存在 group 或目标自身。
/// @return 成功返回 `Ok(())`。
/// @errors 目标不存在返回 NotFound；child 已 exec 返回 Executed；其余约束失败返回 Permission。
pub(crate) fn set_process_group(pid: usize, pgid: usize) -> Result<(), SetProcessGroupError> {
    let caller = current_task()
        .expect("setpgid requires current task")
        .tgid();
    let target = if pid == 0 { caller } else { pid };
    let desired = if pgid == 0 { target } else { pgid };
    let mut graph = TASK_MANAGER.graph.lock();
    let caller_session = graph
        .nodes
        .get(&caller)
        .ok_or(SetProcessGroupError::NotFound)?
        .session;
    let target_node = graph
        .nodes
        .get(&target)
        .ok_or(SetProcessGroupError::NotFound)?;
    if target != caller && target_node.parent != Some(caller) {
        return Err(SetProcessGroupError::NotFound);
    }
    if target_node.session != caller_session || target_node.session == target {
        return Err(SetProcessGroupError::Permission);
    }
    if target != caller && target_node.has_execed {
        return Err(SetProcessGroupError::Executed);
    }
    if desired != target
        && !graph
            .groups
            .get(&(caller_session, desired))
            .is_some_and(|group| !group.members.is_empty())
    {
        return Err(SetProcessGroupError::Permission);
    }
    let old_group = (target_node.session, target_node.process_group);
    if old_group.1 == desired {
        return Ok(());
    }
    let member = graph
        .groups
        .get_mut(&old_group)
        .expect("setpgid old group missing")
        .members
        .take_entry(&target)
        .expect("setpgid member missing from old group");
    let desired_group = (caller_session, desired);
    if graph.groups.contains_key(&desired_group) {
        graph
            .groups
            .get_mut(&desired_group)
            .expect("validated process group disappeared")
            .members
            .commit_vacant(member);
    } else {
        debug_assert_eq!(desired, target);
        let slot = graph
            .nodes
            .get_mut(&target)
            .expect("validated process disappeared under graph lock")
            .group_slot
            .take()
            .expect("new process-group node not reserved");
        let mut members = FallibleMap::new();
        members.commit_vacant(member);
        graph.groups.commit_vacant(slot.fill(
            desired_group,
            ProcessGroupIndex {
                members,
                exit_check_pending: false,
                exit_check_next: None,
                was_orphaned_stopped: false,
            },
        ));
    }
    graph
        .nodes
        .get_mut(&target)
        .expect("validated process disappeared under graph lock")
        .process_group = desired;
    Ok(())
}

/// @description 在 exec point-of-no-return 发布 child 已执行新映像的 process-graph 事实。
///
/// @param tgid 正在提交 exec 的 live Process。
/// @return 无返回值；发布后 parent 的 setpgid 必须返回 EACCES。
pub(in crate::task) fn mark_process_exec(tgid: usize) {
    let mut graph = TASK_MANAGER.graph.lock();
    let node = graph
        .nodes
        .get_mut(&tgid)
        .expect("exec process missing from process graph");
    assert!(matches!(node.state, ProcessState::Live(_)));
    node.has_execed = true;
}

/// @description 当前 session leader 尝试取得一个 Terminal 作为 controlling TTY。
///
/// @param terminal ioctl fd 指向的 TTY owner。
/// @param force TIOCSCTTY force 参数；当前无 capability model，只接受零。
/// @return 成功取得或重复确认同一 session 时返回 `Ok(())`。
/// @errors 非 session leader/force 请求返回 Permission；TTY 属于其他 session 返回 Permission。
pub(crate) fn claim_controlling_terminal(
    terminal: &Arc<crate::fs::Terminal>,
    force: usize,
) -> Result<(), ProcessGroupError> {
    let pid = current_task()
        .expect("TIOCSCTTY requires current task")
        .tgid();
    let (session, pgid) = {
        let graph = TASK_MANAGER.graph.lock();
        let node = graph.nodes.get(&pid).ok_or(ProcessGroupError::NotFound)?;
        (node.session, node.process_group)
    };
    if force != 0 || session != pid {
        return Err(ProcessGroupError::Permission);
    }
    terminal
        .claim_session(session, pgid)
        .map_err(|()| ProcessGroupError::Permission)?;
    current_task()
        .expect("TIOCSCTTY caller disappeared")
        .set_terminal(terminal.clone());
    Ok(())
}

/// @description 查询当前 session controlling TTY 的 foreground process group。
///
/// @param terminal ioctl fd 指向的 TTY owner。
/// @return foreground PGID。
/// @errors fd 的 TTY 不属于 caller session 时返回 NotTerminal。
pub(crate) fn terminal_foreground_group(
    terminal: &crate::fs::Terminal,
) -> Result<usize, ProcessGroupError> {
    let session = session_id(0)?;
    terminal
        .foreground_pgid(session)
        .map_err(|()| ProcessGroupError::NotTerminal)
}

/// @description 将 caller session 内已存在的 process group 设为 TTY foreground owner。
///
/// @param terminal ioctl fd 指向的 TTY owner。
/// @param pgid 同 session 的已存在 process group ID。
/// @return 成功返回 `Ok(())`。
/// @errors group 不存在/跨 session 返回 Permission；TTY 不属于 caller session 返回 NotTerminal。
pub(crate) fn set_terminal_foreground_group(
    terminal: &crate::fs::Terminal,
    pgid: usize,
) -> Result<(), ProcessGroupError> {
    let session = session_id(0)?;
    let graph = TASK_MANAGER.graph.lock();
    if !graph
        .groups
        .get(&(session, pgid))
        .is_some_and(|group| !group.members.is_empty())
    {
        return Err(ProcessGroupError::Permission);
    }
    drop(graph);
    terminal
        .set_foreground_pgid(session, pgid)
        .map_err(|()| ProcessGroupError::NotTerminal)
}

/// @description 在已持有 process graph lock 时计算一个 group 的 POSIX orphan 状态。
///
/// @param graph parent/SID/PGID 与 live state 的唯一 owner。
/// @param session 待检查 group 所属 SID。
/// @param process_group 待检查 PGID。
/// @return 至少一个 live member 且不存在同 session、group 外 live parent connection 时为 true。
pub(super) fn process_group_is_orphaned(
    graph: &ProcessGraph,
    session: usize,
    process_group: usize,
) -> bool {
    let Some(group) = graph.groups.get(&(session, process_group)) else {
        return false;
    };
    let mut has_member = false;
    for (&tgid, ()) in &group.members {
        let node = graph
            .nodes
            .get(&tgid)
            .expect("process-group index references missing process");
        if !matches!(node.state, ProcessState::Live(_)) {
            continue;
        }
        has_member = true;
        let Some(parent) = node.parent.filter(|parent| *parent != INIT_PID) else {
            continue;
        };
        if graph.nodes.get(&parent).is_some_and(|parent| {
            parent.session == session
                && parent.process_group != process_group
                && matches!(parent.state, ProcessState::Live(_))
        }) {
            return false;
        }
    }
    has_member
}

/// @description 查询 Process 当前 group 是否已 orphaned，供默认 stop delivery 复查。
///
/// @param tgid live Process ID。
/// @return group 存在且没有同 session、group 外 live parent connection 时返回 true。
pub(in crate::task) fn current_process_group_is_orphaned(tgid: usize) -> bool {
    let graph = TASK_MANAGER.graph.lock();
    let Some(node) = graph.nodes.get(&tgid) else {
        return true;
    };
    process_group_is_orphaned(&graph, node.session, node.process_group)
}
