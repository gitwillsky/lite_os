use super::*;

/// @description 按 Linux 全局 TID selector 查询 live Thread collection。
///
/// @param graph TaskManager 唯一 process graph 的已锁定快照。
/// @param tid 目标 Thread ID。
/// @return 命中时返回 TGID 与保活的 Thread owner；Exited Process 不参与选择。
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

/// @description 解析 Linux scheduler 的零/current 或正数/global TID selector。
///
/// @param tid 零选择 caller；正数选择全局 live Thread。
/// @param caller calling Thread 的保活 owner。
/// @return 命中时返回保活的目标 Thread；Exited Process 不参与选择。
pub(super) fn scheduler_thread(
    tid: usize,
    caller: &Arc<TaskControlBlock>,
) -> Option<Arc<TaskControlBlock>> {
    if tid == 0 {
        return Some(caller.clone());
    }
    let graph = TASK_MANAGER.graph.lock();
    thread_by_tid(&graph, tid).map(|(_, thread)| thread)
}

/// @description 查询 process graph 中的 parent TGID。
///
/// @param pid 当前 live Process TGID。
/// @return PID 不存在或无 parent 返回零，否则返回 process graph 保存的 parent TGID。
pub(crate) fn parent_pid(pid: usize) -> usize {
    TASK_MANAGER
        .graph
        .lock()
        .nodes
        .get(&pid)
        .and_then(|node| node.parent)
        .unwrap_or(0)
}

/// @description 返回 live Process 当前拥有的 Thread 数量。
///
/// @param tgid 目标 Process TGID。
/// @return Process 不存在或已退出返回零，否则返回 live thread collection 长度。
pub(crate) fn thread_count(tgid: usize) -> usize {
    let graph = TASK_MANAGER.graph.lock();
    match graph.nodes.get(&tgid).map(|node| &node.state) {
        Some(ProcessState::Live(threads)) => threads.len(),
        _ => 0,
    }
}
