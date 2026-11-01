//! sighand disposition 与 pending retirement 的 graph transaction。
use super::*;

/// 冻结 caller Thread group 后原子修改 disposition 和所有同组 pending。
/// 查询无需 graph；replacement 不允许取得/保活其他 Process 的 pending owner。
pub(in crate::task) fn signal_action_for_group(
    task: &TaskControlBlock,
    signal: usize,
    replacement: Option<crate::task::SignalAction>,
) -> Result<crate::task::SignalAction, ()> {
    if replacement.is_none() {
        return task
            .signal_action_for_threads(core::iter::empty(), signal, None, false)
            .map(|(action, retired)| {
                drop(retired);
                action
            });
    }
    let graph = PROCESS_TABLE.graph.lock();
    let Some(ProcessState::Live(threads)) = graph.nodes.get(&task.tgid()).map(|node| &node.state)
    else {
        return Err(());
    };
    let mut result = task.signal_action_for_threads(
        threads.values(),
        signal,
        replacement,
        super::super::ptrace::group_traced(&graph, task.tgid()),
    );
    if let Ok((_, retired)) = &mut result {
        retired
            .recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
    }
    drop(graph);
    if result.is_ok() {
        resume_ignored_posix_timers(task.tgid(), signal);
    }
    result.map(|(action, retired)| {
        drop(retired);
        action
    })
}
