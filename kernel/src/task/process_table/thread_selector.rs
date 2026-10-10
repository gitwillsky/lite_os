use super::*;

/// 按内部稳定 TID key 查询 live Thread collection；ABI 调用方须先解析 namespace 编号。
///
/// # Parameters
///
/// - `graph`: ProcessTable 唯一 process graph 的已锁定快照。
/// - `tid`: 目标 Thread ID。
///
/// # Returns
///
/// 命中时返回 TGID 与保活的 Thread owner；Exited Process 不参与选择。
pub(super) fn thread_by_tid(
    graph: &ProcessGraph,
    tid: usize,
) -> Option<(usize, Arc<TaskControlBlock>)> {
    let tgid = graph.threads.get(&tid)?.tgid;
    let ProcessState::Live(threads) = &graph.nodes.get(&tgid)?.state else {
        return None;
    };
    threads.get(&tid).cloned().map(|thread| (tgid, thread))
}

/// proc 查询 actual execution admission；用户 Process 关系仍只由 thread_by_tid 解析。
/// 返回的 Arc 只观察执行体，不能延长已退休的 PID 或恢复用户资源。
pub(super) fn execution_by_tid(graph: &ProcessGraph, tid: usize) -> Option<Arc<TaskControlBlock>> {
    thread_by_tid(graph, tid)
        .map(|(_, thread)| thread)
        .or_else(|| {
            graph
                .threads
                .get(&tid)
                .and_then(|index| index.retiring_task.clone())
        })
        .or_else(|| graph.kernel_threads.get(&tid).cloned())
}

/// 解析 Linux scheduler 的零/current 或正数/namespace TID selector。
///
/// # Parameters
///
/// - `tid`: 零选择 caller；正数选择 caller PID namespace 中的 live Thread。
/// - `caller`: calling Thread 的保活 owner。
///
/// # Returns
///
/// 命中时返回保活的目标 Thread；Exited Process 不参与选择。
pub(super) fn scheduler_thread(
    tid: usize,
    caller: &Arc<TaskControlBlock>,
) -> Option<Arc<TaskControlBlock>> {
    if tid == 0 {
        return Some(caller.clone());
    }
    let tid = caller.resolve_pid(tid)?;
    let graph = PROCESS_TABLE.graph.lock();
    thread_by_tid(&graph, tid).map(|(_, thread)| thread)
}

/// 在 calling PID namespace 解析 native int TID，再查询同一实际 Thread 的 robust registration。
pub(crate) fn robust_list_head(
    tid: i32,
    caller: &Arc<TaskControlBlock>,
) -> Result<usize, super::super::RobustListError> {
    if tid < 0 {
        return Err(super::super::RobustListError::NotFound);
    }
    if tid == 0 {
        return caller.robust_list_head_for(caller);
    }
    let tid = caller
        .resolve_pid(tid as usize)
        .ok_or(super::super::RobustListError::NotFound)?;
    // 已退出 leader 仍属于 process graph/PID registration；无需保活旧 TCB 才能查询零 head。
    let (thread, process) = {
        let graph = PROCESS_TABLE.graph.lock();
        let thread = execution_by_tid(&graph, tid);
        let process = if thread.is_none() {
            graph
                .nodes
                .get(&tid)
                .map(|node| node.process.retired_inspection_target())
        } else {
            None
        };
        (thread, process)
    };
    // graph guard 已释放；可睡眠的 exec/credential guard 不得反向进入 process graph。
    if let Some(thread) = thread {
        thread.robust_list_head_for(caller)
    } else if let Some(process) = process {
        process.head_for(caller)
    } else {
        Err(super::super::RobustListError::NotFound)
    }
}

/// 查询 process graph 中的 parent TGID。
///
/// # Parameters
///
/// - `pid`: 当前 live Process TGID。
///
/// # Returns
///
/// PID 不存在或无 parent 返回零，否则返回 process graph 保存的 parent TGID。
pub(crate) fn parent_pid(pid: usize) -> usize {
    let namespace = current_task()
        .expect("parent PID requires current task")
        .pid_namespace();
    let graph = PROCESS_TABLE.graph.lock();
    graph
        .nodes
        .get(&pid)
        .and_then(|node| node.parent)
        .and_then(|parent| graph.nodes.get(&parent))
        .map_or(0, |parent| {
            parent.pid.identity().number_in(&namespace) as usize
        })
}

/// Linux current_is_single_threaded：唯一 live Thread 且无其他 Process 共享实际 mm。
/// mm observer 在 graph 锁外取得/归还；锁内不借 strong_count 推断 mm_users。
pub(crate) fn is_single_threaded(task: &TaskControlBlock) -> bool {
    let Some(mm) = task.namespace_mm() else {
        return false;
    };
    let graph = PROCESS_TABLE.graph.lock();
    let Some(ProcessState::Live(own)) = graph.nodes.get(&task.tgid()).map(|node| &node.state)
    else {
        return false;
    };
    if own.len() != 1 {
        return false;
    }
    for (_, node) in graph.nodes.iter() {
        if let ProcessState::Live(threads) = &node.state {
            for (_, other) in threads.iter() {
                if other.tid() != task.tid() && other.shares_namespace_mm(&mm) {
                    return false;
                }
            }
        }
    }
    true
}
