use super::*;

/// 查询或替换 calling Thread 的 Linux parent-death signal。
///
/// # Parameters
///
/// - `replacement`: `Some(signal)` 设置 `0..=64`，`None` 只查询。
///
/// # Returns
///
/// 修改前的 signal。
///
/// # Errors
///
/// signal 越界或 calling Process 已不在 live graph 时返回 `Err(())`。
pub(crate) fn parent_death_signal(replacement: Option<usize>) -> Result<usize, ()> {
    if replacement.is_some_and(|signal| signal > 64) {
        return Err(());
    }
    let task = current_task().ok_or(())?;
    let mut graph = PROCESS_TABLE.graph.lock();
    let Some(node) = graph.nodes.get_mut(&task.tgid()) else {
        return Err(());
    };
    if !matches!(node.state, ProcessState::Live(_)) {
        return Err(());
    }
    let previous = task.parent_death_signal(replacement);
    if let Some(signal) = replacement {
        match (previous == 0, signal == 0) {
            (true, false) => node.pdeath_enabled_threads += 1,
            (false, true) => node.pdeath_enabled_threads -= 1,
            _ => {}
        }
    }
    Ok(previous)
}

fn mark_child_parent_death(graph: &ProcessGraph, child_pid: usize, parent: &TaskControlBlock) {
    let Some(child) = graph.nodes.get(&child_pid) else {
        return;
    };
    let ProcessState::Live(threads) = &child.state else {
        return;
    };
    // creator 已从 live index 摘除；借 exiting Task 的实际 immutable identity/凭据，不反查 retired index。
    let uid = parent.credential_id(true, false);
    for thread in threads.values() {
        let number = parent.tgid_identity().number_in(&thread.pid_namespace());
        thread.mark_parent_death(
            PendingSignal::process_directed(number as usize).with_identity(
                number,
                thread.user_namespace().display_id(true, uid),
                number == 0,
            ),
        );
    }
}

/// 在 parent Thread exit 的 graph transaction 中冻结并重定向 pdeath relation。
///
/// # Parameters
///
/// - `graph`: process relation 与 live Thread 的唯一 owner。
/// - `parent`: 正在退出的实际 creator Thread；其 identity/real UID 冻结 siginfo。
/// - `replacement_tid`: 同 Process 的 live reaper Thread，或全局 init TID。
///
/// # Returns
///
/// 无返回值；signal generation 只写 Thread-owned pending slot，不执行调度操作。
pub(super) fn mark_parent_exit(
    graph: &mut ProcessGraph,
    parent: &TaskControlBlock,
    replacement_tid: usize,
) {
    let parent_tid = parent.tid();
    let index = graph
        .threads
        .get_mut(&parent_tid)
        .expect("exiting creator remains indexed until cleanup completion");
    assert!(index.exit_in_progress);
    let mut created_children = core::mem::take(&mut index.created_children);
    while let Some((&child_pid, _)) = created_children.first_key_value() {
        let membership = created_children
            .take_entry(&child_pid)
            .expect("creator-child membership disappeared");
        let mut stage_pdeath = false;
        let pdeath_enabled = graph
            .nodes
            .get(&child_pid)
            .is_some_and(|child| child.pdeath_enabled_threads != 0);
        if pdeath_enabled {
            mark_child_parent_death(graph, child_pid, parent);
        }
        if let Some(child) = graph.nodes.get_mut(&child_pid) {
            debug_assert_eq!(child.parent_thread, Some(parent_tid));
            if pdeath_enabled {
                stage_pdeath = !child.pdeath_pending;
            }
            child.parent_thread = Some(replacement_tid);
        }
        if stage_pdeath {
            let next = graph.pdeath_head;
            let child = graph
                .nodes
                .get_mut(&child_pid)
                .expect("staged pdeath child disappeared");
            child.pdeath_pending = true;
            child.pdeath_next = next;
            child.pdeath_cursor = 0;
            graph.pdeath_head = Some(child_pid);
        }
        if graph.nodes.contains_key(&child_pid) {
            graph
                .threads
                .get_mut(&replacement_tid)
                .expect("replacement creator thread missing from index")
                .created_children
                .commit_vacant(membership);
        }
    }
}

/// 在 graph lock 外逐项投递已冻结的 process-directed pdeath signal。
///
/// # Returns
///
/// 无返回值；退出中的 target 自然丢弃，不恢复第二套 pending owner。
pub(super) fn drain_parent_death_signals() {
    loop {
        let pending = {
            let mut graph = PROCESS_TABLE.graph.lock();
            let Some(tgid) = graph.pdeath_head else {
                break;
            };
            let (next, cursor) = {
                let node = graph
                    .nodes
                    .get_mut(&tgid)
                    .expect("queued pdeath process disappeared");
                (node.pdeath_next.take(), node.pdeath_cursor)
            };
            graph.pdeath_head = next;
            let event = graph.nodes.get(&tgid).and_then(|node| {
                let ProcessState::Live(threads) = &node.state else {
                    return None;
                };
                threads
                    .iter_after(&cursor)
                    .find_map(|(&tid, thread)| thread.take_parent_death().map(|event| (tid, event)))
            });
            let head = graph.pdeath_head;
            let node = graph
                .nodes
                .get_mut(&tgid)
                .expect("queued pdeath process disappeared after lookup");
            if let Some((tid, (signal, info))) = event {
                node.pdeath_cursor = tid;
                node.pdeath_next = head;
                graph.pdeath_head = Some(tgid);
                Some((tgid, signal, info))
            } else {
                node.pdeath_pending = false;
                node.pdeath_cursor = 0;
                None
            }
        };
        if let Some((tgid, signal, info)) = pending {
            send_kernel_process_signal(tgid, signal, info);
        }
    }
}
