use super::*;

/// @description 查询或替换 calling Thread 的 Linux parent-death signal。
/// @param replacement `Some(signal)` 设置 `0..=64`，`None` 只查询。
/// @return 修改前的 signal。
/// @errors signal 越界或 calling Process 已不在 live graph 时返回 `Err(())`。
pub(crate) fn parent_death_signal(replacement: Option<usize>) -> Result<usize, ()> {
    if replacement.is_some_and(|signal| signal > 64) {
        return Err(());
    }
    let task = current_task().ok_or(())?;
    let graph = TASK_MANAGER.graph.lock();
    if !graph
        .nodes
        .get(&task.tgid())
        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
    {
        return Err(());
    }
    Ok(task.parent_death_signal(replacement))
}

/// @description 在 parent Thread exit 的 graph transaction 中冻结并重定向 pdeath relation。
/// @param graph process relation 与 live Thread 的唯一 owner。
/// @param parent_tgid 退出 Thread 所属 TGID，用作 signal `si_pid`。
/// @param parent_tid 正在退出的 creator Thread ID。
/// @param replacement_tid 同 Process 的 live reaper Thread，或全局 init TID。
/// @return 无返回值；signal generation 只写 Thread-owned pending slot，不执行调度操作。
pub(super) fn mark_parent_exit(
    graph: &mut ProcessGraph,
    parent_tgid: usize,
    parent_tid: usize,
    replacement_tid: usize,
) {
    graph.nodes.for_each_mut(|_, child| {
        if child.parent_thread != Some(parent_tid) {
            return;
        }
        if let ProcessState::Live(threads) = &child.state {
            for thread in threads.values() {
                thread.mark_parent_death(parent_tgid);
            }
        }
        child.parent_thread = Some(replacement_tid);
    });
}

/// @description 在 graph lock 外逐项投递已冻结的 process-directed pdeath signal。
/// @return 无返回值；退出中的 target 自然丢弃，不恢复第二套 pending owner。
pub(super) fn drain_parent_death_signals() {
    loop {
        let pending = {
            let graph = TASK_MANAGER.graph.lock();
            graph.nodes.iter().find_map(|(&tgid, node)| {
                let ProcessState::Live(threads) = &node.state else {
                    return None;
                };
                threads.values().find_map(|thread| {
                    thread
                        .take_parent_death()
                        .map(|(signal, parent)| (tgid, signal, parent))
                })
            })
        };
        let Some((tgid, signal, parent)) = pending else {
            break;
        };
        send_kernel_process_signal(tgid, signal, PendingSignal::process_directed(parent));
    }
}
