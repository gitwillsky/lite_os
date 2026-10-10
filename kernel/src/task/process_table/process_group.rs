use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessGroupError {
    NotFound,
    Permission,
    NotTerminal,
    OutOfMemory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetProcessGroupError {
    NotFound,
    Permission,
    Executed,
    OutOfMemory,
}

/// 将当前 Process 建为新 session 与 process-group leader。
///
/// # Returns
///
/// 成功返回新 SID（等于 TGID）。
///
/// # Errors
///
/// 当前 PID 已是任一 process group ID 时返回 Permission。
pub(crate) fn create_session() -> Result<usize, ProcessGroupError> {
    let task = current_task().expect("setsid requires current task");
    let pid = task.tgid();
    let mut links = None;
    let mut retired = super::group_retirement::RetiredGroups::default();
    loop {
        let mut graph = PROCESS_TABLE.graph.lock();
        if graph.groups.contains_key(&pid) {
            return Err(ProcessGroupError::Permission);
        }
        let old_group = {
            let node = graph.nodes.get(&pid).ok_or(ProcessGroupError::NotFound)?;
            (node.session, node.process_group)
        };
        if links.is_none() {
            drop(graph);
            links = Some((
                PidLinkSlot::try_new().map_err(|_| ProcessGroupError::OutOfMemory)?,
                PidLinkSlot::try_new().map_err(|_| ProcessGroupError::OutOfMemory)?,
            ));
            continue;
        }
        let member = graph
            .groups
            .get_mut(&old_group.1)
            .expect("setsid old group missing")
            .members
            .take_entry(&pid)
            .expect("setsid old membership missing");
        let node = graph.nodes.get_mut(&pid).expect("setsid process missing");
        let slot = node
            .group_slot
            .take()
            .expect("live Process owns vacant group node");
        let (pgid, sid) = links.take().expect("new group links prepared");
        let pgid = node.pid.clone_in(pgid);
        let sid = node.pid.clone_in(sid);
        node.session = pid;
        node.process_group = pid;
        let retired_terminal = node.process.replace_terminal(None);
        let mut members = FallibleMap::new();
        members.commit_vacant(member);
        graph.groups.commit_vacant(slot.fill(
            pid,
            ProcessGroupIndex {
                pgid,
                sid,
                members,
                exit_check_pending: false,
                exit_check_next: None,
                was_orphaned_stopped: false,
            },
        ));
        super::group_retirement::retire_empty_group(&mut graph, old_group, &mut retired);
        drop(graph);
        drop(retired_terminal);
        return Ok(task.visible_tgid() as usize);
    }
}

/// 查询指定 Process 的 process group ID。
///
/// # Parameters
///
/// - `pid`: 零表示当前 TGID，否则为目标 TGID。
///
/// # Returns
///
/// live/zombie process 的 PGID。
///
/// # Errors
///
/// 目标不存在时返回 NotFound。
pub(crate) fn process_group(pid: usize) -> Result<usize, ProcessGroupError> {
    let task = current_task().expect("getpgid requires current task");
    let target = if pid == 0 {
        task.tgid()
    } else {
        task.resolve_pid(pid).ok_or(ProcessGroupError::NotFound)?
    };
    let namespace = task.pid_namespace();
    let graph = PROCESS_TABLE.graph.lock();
    let node = graph
        .nodes
        .get(&target)
        .ok_or(ProcessGroupError::NotFound)?;
    let group = graph
        .groups
        .get(&node.process_group)
        .expect("Process owns actual group membership");
    Ok(group.pgid.identity().number_in(&namespace) as usize)
}

/// 查询 Linux viewer 可见 SID；内部 Terminal ownership 继续使用 stable session key。
pub(crate) fn session_id(pid: usize) -> Result<usize, ProcessGroupError> {
    let task = current_task().expect("getsid requires current task");
    let target = if pid == 0 {
        task.tgid()
    } else {
        task.resolve_pid(pid).ok_or(ProcessGroupError::NotFound)?
    };
    let namespace = task.pid_namespace();
    let graph = PROCESS_TABLE.graph.lock();
    let node = graph
        .nodes
        .get(&target)
        .ok_or(ProcessGroupError::NotFound)?;
    let group = graph
        .groups
        .get(&node.process_group)
        .expect("Process owns actual group membership");
    Ok(group.sid.identity().number_in(&namespace) as usize)
}

/// 按 Linux parent/child/session/exec 约束修改 process group membership。
///
/// # Parameters
///
/// - `pid`: 零表示 caller；非零只允许 caller 的直接 child。
/// - `pgid`: 零表示目标 TGID；非零必须是同 session 已存在 group 或目标自身。
///
/// # Returns
///
/// 成功返回 `Ok(())`。
///
/// # Errors
///
/// 目标不存在返回 NotFound；child 已 exec 返回 Executed；其余约束失败返回 Permission。
pub(crate) fn set_process_group(pid: usize, pgid: usize) -> Result<(), SetProcessGroupError> {
    let task = current_task().expect("setpgid requires current task");
    let caller = task.tgid();
    let target = if pid == 0 {
        caller
    } else {
        task.resolve_pid(pid)
            .ok_or(SetProcessGroupError::NotFound)?
    };
    let desired = if pgid == 0 {
        target
    } else {
        task.resolve_pid(pgid)
            .ok_or(SetProcessGroupError::Permission)?
    };
    let mut links = None;
    let mut retired = super::group_retirement::RetiredGroups::default();
    loop {
        let mut graph = PROCESS_TABLE.graph.lock();
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
        if target != caller && target_node.exec_generation != 0 {
            return Err(SetProcessGroupError::Executed);
        }
        let existing = match graph.groups.get(&desired) {
            Some(group) if group.sid.identity().key() == caller_session => true,
            Some(_) => return Err(SetProcessGroupError::Permission),
            None => false,
        };
        if desired != target && !existing {
            return Err(SetProcessGroupError::Permission);
        }
        let old_group = (target_node.session, target_node.process_group);
        if old_group.1 == desired {
            return Ok(());
        }
        if !existing && links.is_none() {
            drop(graph);
            links = Some((
                PidLinkSlot::try_new().map_err(|_| SetProcessGroupError::OutOfMemory)?,
                PidLinkSlot::try_new().map_err(|_| SetProcessGroupError::OutOfMemory)?,
            ));
            continue;
        }
        let member = graph
            .groups
            .get_mut(&old_group.1)
            .expect("setpgid old group missing")
            .members
            .take_entry(&target)
            .expect("setpgid membership missing");
        if existing {
            graph
                .groups
                .get_mut(&desired)
                .expect("validated group missing")
                .members
                .commit_vacant(member);
        } else {
            let (pgid, sid) = links.take().expect("new group links prepared");
            let sid = graph
                .groups
                .get(&old_group.1)
                .expect("source group missing")
                .sid
                .clone_in(sid);
            let node = graph
                .nodes
                .get_mut(&target)
                .expect("setpgid target missing");
            let pgid = node.pid.clone_in(pgid);
            let slot = node
                .group_slot
                .take()
                .expect("live Process owns vacant group node");
            let mut members = FallibleMap::new();
            members.commit_vacant(member);
            graph.groups.commit_vacant(slot.fill(
                desired,
                ProcessGroupIndex {
                    pgid,
                    sid,
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
            .expect("setpgid target missing")
            .process_group = desired;
        super::group_retirement::retire_empty_group(&mut graph, old_group, &mut retired);
        return Ok(());
    }
}

/// 清除指定 session 实际 members 对同一 Terminal 的 handle；不扫描不相关 Process。
///
/// caller 在事务外 pin Terminal，因此每次清除都只减少非最后 Arc，不触发 allocator 或设备 Drop。
/// SID membership 沿实际 PGID groups/member index 解释；无第二份 session registry。
pub(super) fn clear_terminal_session(
    graph: &ProcessGraph,
    session: usize,
    terminal: &crate::tty::Terminal,
) {
    for group in graph
        .groups
        .values()
        .filter(|group| group.sid.identity().key() == session)
    {
        for (&member, ()) in group.members.iter() {
            if let Some(node) = graph.nodes.get(&member) {
                node.process.clear_terminal_if(terminal);
            }
        }
    }
}

/// TIOCSCTTY 按实际 session/Process TTY owner 与 initial CAP_SYS_ADMIN 完成控制权事务。
///
/// # Errors
///
/// 非 session leader、已有另一 controlling TTY、无权接管或只写 fd 无 global SYS_ADMIN 为 EPERM。
/// 不分配，旧控制 PID pins 在整个 graph publication 之后退休。
pub(super) enum TerminalClaim {
    Open { readable: bool },
    Ioctl { force: usize, readable: bool },
}

/// open 自动取得与 TIOCSCTTY 共享同一 graph/control publication；自动取得不偷取或报权限错误。
pub(super) fn claim_controlling_terminal(
    terminal: &Arc<crate::tty::Terminal>,
    claim: TerminalClaim,
) -> Result<(), ProcessGroupError> {
    let task = current_task().expect("TTY claim requires current task");
    let pid = task.tgid();
    let privileged = task.has_initial_capability(syscall_abi::capability::Capability::SYS_ADMIN);
    let graph = PROCESS_TABLE.graph.lock();
    let node = graph.nodes.get(&pid).ok_or(ProcessGroupError::NotFound)?;
    let owner = terminal.controlling_session();
    let steal = match claim {
        TerminalClaim::Open { readable } => {
            if node.session != pid
                || task.terminal().is_some()
                || owner.is_some()
                || !readable && !privileged
            {
                return Ok(());
            }
            false
        }
        TerminalClaim::Ioctl { force, readable } => {
            if node.session != pid {
                return Err(ProcessGroupError::Permission);
            }
            // 同 session 重复确认不重置 foreground PGID，也不再次判 force/FD mode。
            if owner == Some(pid) {
                return Ok(());
            }
            if task.terminal().is_some() || !readable && !privileged {
                return Err(ProcessGroupError::Permission);
            }
            force as i32 == 1 && privileged
        }
    };
    let group = graph
        .groups
        .get(&node.process_group)
        .expect("TTY caller owns process group");
    let retired_control = terminal
        .claim_session(
            group.sid.identity().clone(),
            group.pgid.identity().clone(),
            steal,
        )
        .map_err(|()| ProcessGroupError::Permission)?;
    if let Some(previous) = retired_control.previous_session() {
        clear_terminal_session(&graph, previous, terminal);
    }
    assert!(
        node.process
            .replace_terminal(Some(terminal.clone()))
            .is_none()
    );
    drop(graph);
    drop(retired_control);
    Ok(())
}

/// TIOCNOTTY：普通成员只清自身；session leader 脱离整个 session 并通知前台 group。
///
/// # Errors
///
/// fd 不是 caller 的实际 controlling terminal 返回 NotTerminal；不分配或保留第二份状态。
pub(super) fn detach_controlling_terminal(
    terminal: &crate::tty::Terminal,
) -> Result<(), ProcessGroupError> {
    let task = current_task().expect("TIOCNOTTY requires current task");
    let graph = PROCESS_TABLE.graph.lock();
    let node = graph
        .nodes
        .get(&task.tgid())
        .ok_or(ProcessGroupError::NotFound)?;
    if !task
        .terminal()
        .is_some_and(|current| core::ptr::eq(current.as_ref(), terminal))
    {
        return Err(ProcessGroupError::NotTerminal);
    }
    let retired = if node.session == task.tgid() {
        let retired = terminal.hangup();
        clear_terminal_session(&graph, node.session, terminal);
        Some(retired)
    } else {
        node.process.clear_terminal_if(terminal);
        None
    };
    drop(graph);
    if let Some(group) = retired
        .as_ref()
        .and_then(|control| control.foreground_group())
    {
        send_process_group_signal(group, syscall_abi::signal::SIGHUP);
        send_process_group_signal(group, syscall_abi::signal::SIGCONT);
    }
    drop(retired);
    Ok(())
}

/// 将 caller session 内已存在的 process group 设为 TTY foreground owner。
///
/// # Parameters
///
/// - `terminal`: ioctl fd 指向的 TTY owner。
/// - `pgid`: 同 session 的已存在 process group ID。
///
/// # Returns
///
/// 成功返回 `Ok(())`。
///
/// # Errors
///
/// group 不存在/跨 session 返回 Permission；TTY 不属于 caller session 返回 NotTerminal。
pub(super) fn set_terminal_foreground_group(
    terminal: &crate::tty::Terminal,
    pgid: usize,
) -> Result<(), ProcessGroupError> {
    let task = current_task().expect("TIOCSPGRP requires current task");
    let graph = PROCESS_TABLE.graph.lock();
    let session = graph
        .nodes
        .get(&task.tgid())
        .ok_or(ProcessGroupError::NotFound)?
        .session;
    if !task
        .terminal()
        .is_some_and(|current| core::ptr::eq(current.as_ref(), terminal))
        || terminal.controlling_session() != Some(session)
    {
        return Err(ProcessGroupError::NotTerminal);
    }
    let namespace = task.pid_namespace();
    let pgid = namespace
        .lookup(pgid as u32)
        .ok_or(ProcessGroupError::NotFound)?
        .identity();
    let group = graph
        .groups
        .get(&pgid)
        .filter(|group| !group.members.is_empty())
        .ok_or(ProcessGroupError::NotFound)?;
    if group.sid.identity().key() != session {
        return Err(ProcessGroupError::Permission);
    }
    let identity = group.pgid.identity().clone();
    // 保持 graph -> Terminal 锁序，TTY 接管不能穿过 SID 校验与 PGID publication。
    terminal
        .set_foreground_pgid(session, identity)
        .map_err(|()| ProcessGroupError::NotTerminal)
}

/// 在已持有 process graph lock 时计算一个 group 的 POSIX orphan 状态。
///
/// # Parameters
///
/// - `graph`: parent/SID/PGID 与 live state 的唯一 owner。
/// - `session`: 待检查 group 所属 SID。
/// - `process_group`: 待检查 PGID。
///
/// # Returns
///
/// 至少一个 live member 且不存在同 session、group 外 live parent connection 时为 true。
pub(super) fn process_group_is_orphaned(
    graph: &ProcessGraph,
    session: usize,
    process_group: usize,
) -> bool {
    let Some(group) = graph.groups.get(&process_group) else {
        return false;
    };
    if group.sid.identity().key() != session {
        return false;
    }
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

/// 查询 Process 当前 group 是否已 orphaned，供默认 stop delivery 复查。
///
/// # Parameters
///
/// - `tgid`: live Process ID。
///
/// # Returns
///
/// group 存在且没有同 session、group 外 live parent connection 时返回 true。
pub(in crate::task) fn current_process_group_is_orphaned(tgid: usize) -> bool {
    let graph = PROCESS_TABLE.graph.lock();
    let Some(node) = graph.nodes.get(&tgid) else {
        return true;
    };
    process_group_is_orphaned(&graph, node.session, node.process_group)
}
