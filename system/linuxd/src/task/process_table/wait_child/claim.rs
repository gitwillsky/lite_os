//! child event 的消费/观察 claim 事务；退出时已决定的 autoreap 不重新读取 parent policy。
use super::*;

fn wake_rechecking_waiters(mut waiters: FallibleMap<usize, Arc<TaskControlBlock>>) {
    while let Some((&tid, _)) = waiters.first_key_value() {
        let waiter = waiters.remove(&tid).expect("staged child waiter");
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
}

/// WNOWAIT 观察结束时释放唯一 child-event claim，使其他 Thread 可重新观察该事件。
///
/// # Parameters
///
/// - `record`: `wait_child` 返回且尚未消费的 claim token。
///
/// # Returns
///
/// 无返回值；claim 不匹配表示 wait transaction 被重复结束并 fail-stop。
pub(crate) fn release_child_status(record: ChildExit) {
    if matches!(
        record.kind,
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit
    ) {
        super::super::ptrace::finish_claim(record, false);
        return;
    }
    let (waiters, retired) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let parent = {
            let node = graph
                .nodes
                .get_mut(&record.pid)
                .expect("released child event disappeared from process graph");
            assert_eq!(
                node.child_wait_claim.take(),
                Some(ChildWaitClaim {
                    waiter: record.claimant,
                    kind: record.kind,
                }),
                "child event release does not own claim"
            );
            node.parent
        };
        let mut retired = super::super::process_retirement::RetiredProcesses::default();
        if graph.nodes.get(&record.pid).is_some_and(|node| {
            matches!(
                node.state,
                ProcessState::Exited(_, ExitRetirement::AfterClaim)
            )
        }) {
            retired.retire(&mut graph, record.pid);
        }
        let waiters = parent
            .and_then(|pid| graph.nodes.get_mut(&pid))
            .map(take_child_waiters)
            .unwrap_or_default();
        (waiters, retired)
    };
    drop(retired);
    wake_rechecking_waiters(waiters);
}

/// 在 status copyout 前消费唯一 child event 或 exit record；后续 EFAULT 不恢复事件。
///
/// # Parameters
///
/// - `record`: `wait_child` 返回且仍属于当前 parent 的 record。
///
/// # Returns
///
/// 无返回值；record 变化表示 process graph 不变量损坏。
pub(crate) fn consume_child_status(record: ChildExit) {
    if matches!(
        record.kind,
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit
    ) {
        super::super::ptrace::finish_claim(record, true);
        return;
    }
    let (waiters, retired) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let parent = {
            let node = graph
                .nodes
                .get_mut(&record.pid)
                .expect("reaped child missing from process graph");
            assert_eq!(
                node.child_wait_claim.take(),
                Some(ChildWaitClaim {
                    waiter: record.claimant,
                    kind: record.kind,
                }),
                "child event consume does not own claim"
            );
            match record.kind {
                ChildStatusKind::Exited => {
                    assert!(matches!(node.state, ProcessState::Exited(..)));
                    assert!(node.child_waiters.is_empty());
                }
                ChildStatusKind::Stopped => {
                    assert!(node.child_events.stopped.take().is_some());
                }
                ChildStatusKind::TracedStop | ChildStatusKind::TracedExit => unreachable!(),
                ChildStatusKind::Continued => {
                    assert!(core::mem::take(&mut node.child_events.continued));
                }
            }
            node.parent
        };
        if record.kind == ChildStatusKind::Exited {
            absorb_reaped_usage(&graph, record.pid, parent);
        }
        let mut retired = super::super::process_retirement::RetiredProcesses::default();
        if record.kind == ChildStatusKind::Exited
            || graph.nodes.get(&record.pid).is_some_and(|node| {
                matches!(
                    node.state,
                    ProcessState::Exited(_, ExitRetirement::AfterClaim)
                )
            })
        {
            retired.retire(&mut graph, record.pid);
        }
        let waiters = parent
            .and_then(|pid| graph.nodes.get_mut(&pid))
            .map(take_child_waiters)
            .unwrap_or_default();
        (waiters, retired)
    };
    // 最终 credential/group/graph node 析构不可进入 IRQ graph guard。
    drop(retired);
    wake_rechecking_waiters(waiters);
}
