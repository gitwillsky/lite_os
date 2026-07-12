use super::*;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildExit {
    pub(crate) pid: usize,
    pub(crate) status: i32,
    kind: ChildStatusKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildStatusKind {
    Exited,
    Stopped,
    Continued,
}

/// @description wait4 在 task layer 的精确结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitChildError {
    NoChild,
    InvalidSelector,
    Interrupted,
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
    graph: &ProcessGraph,
    parent: usize,
    selector: isize,
    include_stopped: bool,
    include_continued: bool,
) -> Result<Option<ChildExit>, WaitChildError> {
    let mut has_child = false;
    for (pid, node) in &graph.nodes {
        if node.parent != Some(parent) || !matching_child(graph, parent, *pid, node, selector)? {
            continue;
        }
        has_child = true;
        if let ProcessState::Exited(code) = node.state {
            return Ok(Some(ChildExit {
                pid: *pid,
                status: (code & 0xff) << 8,
                kind: ChildStatusKind::Exited,
            }));
        }
        if include_stopped && let Some(signal) = node.child_events.stopped {
            return Ok(Some(ChildExit {
                pid: *pid,
                status: ((signal as i32) << 8) | 0x7f,
                kind: ChildStatusKind::Stopped,
            }));
        }
        if include_continued && node.child_events.continued {
            return Ok(Some(ChildExit {
                pid: *pid,
                status: 0xffff,
                kind: ChildStatusKind::Continued,
            }));
        }
    }
    has_child.then_some(None).ok_or(WaitChildError::NoChild)
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
        let mut graph = TASK_MANAGER.graph.lock();
        match find_waitable_child(&graph, parent, selector, include_stopped, include_continued)? {
            Some(record) => return Ok(Some(record)),
            None if nohang => return Ok(None),
            None => {}
        }
        if task.has_deliverable_signal() {
            return Err(WaitChildError::Interrupted);
        }

        let cpu = hart_id();
        let end_time = get_time_us();
        let mut sched = task.scheduling.policy.lock();
        let runtime = end_time.saturating_sub(sched.last_runtime);
        sched.update_vruntime(runtime);
        drop(sched);

        // graph lock 覆盖 child 复查与 waiter 发布；exit/job event 使用同一 owner，因此不会丢唤醒。
        with_current_processor(|processor| {
            let current = processor
                .take_current()
                .expect("child wait requires current task");
            assert!(Arc::ptr_eq(&current, &task));
            let mut scheduling = task.scheduling.state.lock();
            assert_eq!(scheduling.run_state, RunState::Running { cpu });
            assert!(
                scheduling.wait.is_none(),
                "task already owns wait membership"
            );
            assert!(scheduling.wait_result.is_none());
            let parent_node = graph
                .nodes
                .get_mut(&parent)
                .expect("waiting parent missing from process graph");
            assert!(
                parent_node.waiter.is_none(),
                "parent already owns child waiter"
            );
            parent_node.waiter = Some(current);
            scheduling.wait = Some(WaitMembership::Child);
            scheduling.run_state = RunState::Blocking { cpu };
        });
        drop(graph);
        schedule_with_task_context(task.clone());
        match task
            .scheduling
            .state
            .lock()
            .wait_result
            .take()
            .expect("child waiter resumed without a wake result")
        {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(WaitChildError::Interrupted),
            WaitResult::TimedOut => panic!("child waiter cannot time out"),
        }
    }
}

/// @description status copyout 成功后消费唯一 child event 或 exit record。
///
/// @param record `wait_child` 返回且仍属于当前 parent 的 record。
/// @return 无返回值；record 变化表示 process graph 不变量损坏。
pub(crate) fn consume_child_status(record: ChildExit) {
    let parent = current_task().expect("reap requires current task").tgid();
    let mut graph = TASK_MANAGER.graph.lock();
    let node = graph
        .nodes
        .get_mut(&record.pid)
        .expect("reaped child missing from process graph");
    assert_eq!(node.parent, Some(parent));
    match record.kind {
        ChildStatusKind::Exited => {
            assert!(matches!(node.state, ProcessState::Exited(_)));
            assert!(node.waiter.is_none());
            graph.nodes.remove(&record.pid);
        }
        ChildStatusKind::Stopped => {
            assert!(node.child_events.stopped.take().is_some());
        }
        ChildStatusKind::Continued => {
            assert!(core::mem::take(&mut node.child_events.continued));
        }
    }
}
