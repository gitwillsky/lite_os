use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildStatusKind {
    Exited,
    Stopped,
    Continued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChildWaitClaim {
    pub(super) waiter: usize,
    pub(super) kind: ChildStatusKind,
}

/// @description 取走 parent Process 的全部 child-event waiter，由调用者在 graph 锁外唤醒。
///
/// @param node process graph 内的 parent node。
/// @return 以全局 TID 去重的 waiter owner；各 Thread 醒来后按自己的 selector 重新检查。
pub(super) fn take_child_waiters(
    node: &mut ProcessNode,
) -> FallibleMap<usize, Arc<TaskControlBlock>> {
    core::mem::take(&mut node.child_waiters)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildExit {
    pub(crate) pid: usize,
    pub(crate) status: i32,
    kind: ChildStatusKind,
    claimant: usize,
}

/// @description wait4 在 task layer 的精确结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitChildError {
    NoChild,
    InvalidSelector,
    Interrupted,
    OutOfMemory,
}

fn matching_child(
    graph: &ProcessGraph,
    parent: usize,
    child: usize,
    node: &ProcessNode,
    selector: isize,
) -> Result<bool, WaitChildError> {
    match selector {
        -1 => Ok(true),
        value if value > 0 => Ok(child == value as usize),
        0 => Ok(node.process_group == graph.nodes[&parent].process_group),
        value => value
            .checked_neg()
            .map(|group| node.process_group == group as usize)
            .ok_or(WaitChildError::InvalidSelector),
    }
}

fn find_waitable_child(
    graph: &mut ProcessGraph,
    parent: usize,
    claimant: usize,
    selector: isize,
    include_stopped: bool,
    include_continued: bool,
) -> Result<Option<ChildExit>, WaitChildError> {
    let mut has_child = false;
    let mut selected = None;
    let mut cursor = 0;
    loop {
        let pid = match selector {
            value if value > 0 && cursor == 0 => Some(value as usize),
            value if value > 0 => break,
            _ => graph
                .nodes
                .get(&parent)
                .and_then(|node| node.children.successor(&cursor))
                .map(|(&pid, _)| pid),
        };
        let Some(pid) = pid else {
            break;
        };
        cursor = pid;
        let Some(node) = graph.nodes.get(&pid) else {
            // Positive waitpid selector is a caller-supplied PID, not a parent-index node. A
            // competing parent Thread may already have consumed that child's unique exit claim;
            // Linux requires the loser to observe ECHILD, not an index-corruption panic.
            if selector > 0 {
                break;
            }
            panic!("parent child index references missing process");
        };
        if node.parent != Some(parent) || !matching_child(graph, parent, pid, node, selector)? {
            if selector > 0 {
                break;
            }
            continue;
        }
        has_child = true;
        if node.child_wait_claim.is_some() {
            continue;
        }
        if let ProcessState::Exited(status) = node.state {
            selected = Some(ChildExit {
                pid,
                status: status.wait_status(),
                kind: ChildStatusKind::Exited,
                claimant,
            });
            break;
        }
        if include_stopped && let Some(signal) = node.child_events.stopped {
            selected = Some(ChildExit {
                pid,
                status: ((signal as i32) << 8) | 0x7f,
                kind: ChildStatusKind::Stopped,
                claimant,
            });
            break;
        }
        if include_continued && node.child_events.continued {
            selected = Some(ChildExit {
                pid,
                status: 0xffff,
                kind: ChildStatusKind::Continued,
                claimant,
            });
            break;
        }
    }
    let Some(record) = selected else {
        return if has_child {
            Ok(None)
        } else {
            Err(WaitChildError::NoChild)
        };
    };
    let node = graph
        .nodes
        .get_mut(&record.pid)
        .expect("selected child disappeared while graph is locked");
    assert!(
        node.child_wait_claim
            .replace(ChildWaitClaim {
                waiter: claimant,
                kind: record.kind,
            })
            .is_none(),
        "child event claimed twice"
    );
    Ok(Some(record))
}

/// @description 等待直接 child 的 exit、stopped 或 continued 状态。
///
/// @param selector `>0` 为 PID，`-1` 为任一 child，`0`/`<-1` 为 process group。
/// @param nohang 无可消费 record 时是否立即返回。
/// @param include_stopped 是否消费尚未报告的 job-control stop。
/// @param include_continued 是否消费尚未报告的 continue。
/// @return child record、WNOHANG 的 None，或 selector/child/interruption 错误。
pub(crate) fn wait_child(
    selector: isize,
    nohang: bool,
    include_stopped: bool,
    include_continued: bool,
) -> Result<Option<ChildExit>, WaitChildError> {
    let task = current_task().expect("wait4 requires current task");
    let parent = task.tgid();
    loop {
        // Storage is prepared outside the graph owner. Event/nohang/signal decisions remain
        // successful on OOM; only publishing a blocking waiter requires this allocation.
        let waiter = FallibleMap::<usize, Arc<TaskControlBlock>>::try_reserve_node();
        let mut graph = TASK_MANAGER.graph.lock();
        let record = find_waitable_child(
            &mut graph,
            parent,
            task.tid(),
            selector,
            include_stopped,
            include_continued,
        )?;
        match super::wait_publication::child_wait_publication(
            record.is_some(),
            nohang,
            task.has_deliverable_signal(),
            waiter.is_ok(),
        ) {
            super::wait_publication::ChildWaitPublication::ConsumeEvent => return Ok(record),
            super::wait_publication::ChildWaitPublication::ReturnNoHang => return Ok(None),
            super::wait_publication::ChildWaitPublication::Interrupted => {
                return Err(WaitChildError::Interrupted);
            }
            super::wait_publication::ChildWaitPublication::OutOfMemory => {
                return Err(WaitChildError::OutOfMemory);
            }
            super::wait_publication::ChildWaitPublication::Publish => {}
        }

        // graph lock 覆盖 child 复查与 waiter 发布；exit/job event 使用同一 owner，因此不会丢唤醒。
        let waiter = waiter
            .expect("publication decision requires prepared child waiter")
            .fill(task.tid(), task.clone());
        let prepared =
            super::context_switch::prepare_current_block(&task, graph, move |graph, _| {
                let parent_node = graph
                    .nodes
                    .get_mut(&parent)
                    .expect("waiting parent missing from process graph");
                parent_node.child_waiters.commit_vacant(waiter);
                WaitMembership::Child
            });
        match prepared.suspend() {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(WaitChildError::Interrupted),
            WaitResult::TimedOut => panic!("child waiter cannot time out"),
            WaitResult::OutOfMemory => unreachable!("child wait has no indexed allocation"),
        }
    }
}

fn wake_rechecking_waiters(mut waiters: FallibleMap<usize, Arc<TaskControlBlock>>) {
    while let Some((&tid, _)) = waiters.first_key_value() {
        let waiter = waiters.remove(&tid).expect("staged child waiter");
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
}

/// @description copyout 失败时释放唯一 child-event claim，使其他 Thread 可重新观察该事件。
///
/// @param record `wait_child` 返回且尚未消费的 claim token。
/// @return 无返回值；claim 不匹配表示 wait transaction 被重复结束并 fail-stop。
pub(crate) fn release_child_status(record: ChildExit) {
    let waiters = {
        let mut graph = TASK_MANAGER.graph.lock();
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
        parent
            .and_then(|pid| graph.nodes.get_mut(&pid))
            .map(take_child_waiters)
            .unwrap_or_default()
    };
    wake_rechecking_waiters(waiters);
}

/// @description status copyout 成功后消费唯一 child event 或 exit record。
///
/// @param record `wait_child` 返回且仍属于当前 parent 的 record。
/// @return 无返回值；record 变化表示 process graph 不变量损坏。
pub(crate) fn consume_child_status(record: ChildExit) {
    let waiters = {
        let mut graph = TASK_MANAGER.graph.lock();
        let (parent, parent_thread, session, process_group) = {
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
                    assert!(matches!(node.state, ProcessState::Exited(_)));
                    assert!(node.child_waiters.is_empty());
                }
                ChildStatusKind::Stopped => {
                    assert!(node.child_events.stopped.take().is_some());
                }
                ChildStatusKind::Continued => {
                    assert!(core::mem::take(&mut node.child_events.continued));
                }
            }
            (
                node.parent,
                node.parent_thread,
                node.session,
                node.process_group,
            )
        };
        if record.kind == ChildStatusKind::Exited {
            graph.nodes.remove(&record.pid);
            if let Some(parent) = parent {
                graph
                    .nodes
                    .get_mut(&parent)
                    .expect("reaped child parent missing from graph")
                    .children
                    .remove(&record.pid)
                    .expect("reaped child missing from parent index");
            }
            if let Some(parent_thread) = parent_thread
                && let Some(index) = graph.threads.get_mut(&parent_thread)
            {
                index.created_children.remove(&record.pid);
            }
            graph
                .groups
                .get_mut(&(session, process_group))
                .expect("reaped child process group missing from graph")
                .members
                .remove(&record.pid)
                .expect("reaped child missing from group index");
        }
        parent
            .and_then(|pid| graph.nodes.get_mut(&pid))
            .map(take_child_waiters)
            .unwrap_or_default()
    };
    wake_rechecking_waiters(waiters);
}
