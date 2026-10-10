//! Namespace init 的唯一 shutdown event；生命周期与 local member index 仍归 PidNamespace。
use super::*;
pub(super) struct NamespaceExit(crate::sync::TaskEvent);
impl NamespaceExit {
    pub(super) const fn new() -> Self {
        Self(crate::sync::TaskEvent::new())
    }
}
impl crate::namespace::PidExitWake for NamespaceExit {
    fn signal(&self) {
        self.0.signal();
    }
}

/// exit_in_progress 与最后 reaper Thread 的 shutdown claim 在唯一 graph guard 下线性化。
/// 包含已开始 cleanup、尚未摘图的 sibling，不能只在先前读到 threads.len()==1 时判断。
pub(super) fn mark_reaper_exit(
    task: &TaskControlBlock,
    namespace: &Arc<crate::namespace::PidNamespace>,
) -> Option<Arc<NamespaceExit>> {
    let mut graph = PROCESS_TABLE.graph.lock();
    graph
        .threads
        .get_mut(&task.tid())
        .expect("exiting Thread is indexed")
        .exit_in_progress = true;
    let node = graph
        .nodes
        .get(&task.tgid())
        .expect("exiting Process is indexed");
    let ProcessState::Live(threads) = &node.state else {
        panic!("Process exited twice");
    };
    if !node.pid.identity().is_namespace_init()
        || namespace.parent().is_none()
        || !threads.values().all(|thread| {
            graph
                .threads
                .get(&thread.tid())
                .expect("live Thread is indexed")
                .exit_in_progress
        })
    {
        return None;
    }
    let event = node
        .namespace_exit
        .as_ref()
        .expect("namespace init owns shutdown event")
        .clone();
    let wake: Arc<dyn crate::namespace::PidExitWake> = event.clone();
    namespace.disable(
        crate::namespace::PidKey::from_identity(task.tgid()),
        Some(Arc::downgrade(&wake)),
    );
    Some(event)
}

/// 禁止新成员后只沿该 namespace 的实际编号 index 清退，等待全部 association 回收。
pub(super) fn shutdown(
    task: &TaskControlBlock,
    namespace: &Arc<crate::namespace::PidNamespace>,
    event: &NamespaceExit,
) {
    super::signal::signal_action_for_group(
        task,
        syscall_abi::signal::SIGCHLD,
        Some(crate::task::SignalAction {
            handler: 1,
            ..crate::task::SignalAction::default()
        }),
    )
    .expect("live namespace init retains signal handlers");
    let mut cursor = 1;
    while let Some((number, key)) = namespace.next_member(cursor) {
        cursor = number;
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::thread_by_tid(&graph, key.identity())
                .filter(|(tgid, _)| *tgid != task.tgid())
                .map(|(_, thread)| thread)
        };
        if let Some(target) = target {
            super::process_exit::request_group_exit(
                &target,
                super::process_exit::ProcessExitStatus::Signaled(
                    syscall_abi::signal::SIGKILL as u8,
                ),
                false,
                true,
            );
        }
    }
    let init_numbers = if task.tid() == task.tgid() { 1 } else { 2 };
    loop {
        match super::wait_child::wait_child(
            -1,
            true,
            false,
            false,
            super::wait_child::ChildWaitFilter {
                class: super::wait_child::ChildWaitClass::All,
                creator_only: false,
            },
        ) {
            Ok(Some(record)) => {
                super::wait_child::consume_child_status(record);
                continue;
            }
            Ok(None) | Err(super::wait_child::WaitChildError::NoChild) => {}
            other => panic!("nonblocking namespace reap failed: {other:?}"),
        }
        if namespace.reaper_only(init_numbers) {
            return;
        }
        // auto-reap 与 external-parent reap 的实际 association retirement signal 同一事件；
        // pending handshake 保证检查→登记之间没有丢唤醒，退出不能被自身 SIGKILL 取消。
        event.0.wait();
    }
}

/// 在 caller 持 graph guard 时选最近的实际 namespace reaper；返回其 TGID 与 live Thread key。
pub(super) fn nearest_reaper(graph: &ProcessGraph, exiting: usize) -> (usize, usize) {
    let mut namespace = Some(
        graph
            .nodes
            .get(&exiting)
            .expect("exiting Process is indexed")
            .pid
            .identity()
            .namespace(),
    );
    while let Some(current) = namespace {
        if let Some(key) = current
            .reaper()
            .map(|key| key.identity())
            .filter(|key| *key != exiting)
            && let Some(node) = graph.nodes.get(&key)
            && let ProcessState::Live(threads) = &node.state
            && let Some((&tid, _)) = threads.iter().find(|(tid, _)| {
                !graph
                    .threads
                    .get(tid)
                    .expect("live Thread is indexed")
                    .exit_in_progress
            })
        {
            return (key, tid);
        }
        namespace = current.parent();
    }
    assert_eq!(
        exiting, INIT_PID,
        "noninitial PID namespace has an ancestor reaper"
    );
    (INIT_PID, INIT_PID)
}
