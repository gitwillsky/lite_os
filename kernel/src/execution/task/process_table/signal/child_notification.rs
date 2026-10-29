use super::*;

use crate::execution::task::model::PreparedSignalRecord;

#[derive(Clone, Copy)]
pub(in crate::execution::task::process_table) enum ChildNotificationScope {
    Parent,
    Exit,
    ExitAdoption,
}
struct PreparedChildRecipient {
    thread: Arc<TaskControlBlock>,
    signal: usize,
    record: PreparedSignalRecord,
}
/// child notification 的两个准确 recipient owner；准备在 graph 外，recipient/signal 改变时重试。
/// 不缓存 disposition，最终 notify/autoreap 与 pending 仍在原 graph/table 事务提交。
pub(in crate::execution::task::process_table) struct PreparedChildNotifications {
    parent: Option<PreparedChildRecipient>,
    adoption: Option<PreparedChildRecipient>,
}
impl PreparedChildNotifications {
    pub(in crate::execution::task::process_table) fn capture(
        graph: &ProcessGraph,
        child: usize,
        scope: ChildNotificationScope,
    ) -> Self {
        let parent = graph.nodes.get(&child).and_then(|node| {
            let parent = node.parent?;
            let creator = node.parent_thread?;
            let ProcessState::Live(threads) = &graph.nodes.get(&parent)?.state else {
                return None;
            };
            threads.get(&creator).cloned().map(|thread| {
                let signal = match scope {
                    ChildNotificationScope::Parent => syscall_abi::signal::SIGCHLD,
                    ChildNotificationScope::Exit | ChildNotificationScope::ExitAdoption => {
                        exit_notification_signal(graph, node)
                    }
                };
                (thread, signal)
            })
        });
        let adoption = match scope {
            ChildNotificationScope::Parent | ChildNotificationScope::Exit => None,
            ChildNotificationScope::ExitAdoption => {
                let (parent, tid) = super::super::pid_namespace::nearest_reaper(graph, child);
                graph.nodes.get(&parent).and_then(|node| match &node.state {
                    ProcessState::Live(threads) => threads.get(&tid).cloned(),
                    _ => None,
                })
            }
        };
        let adoption = adoption.filter(|thread| {
            parent.as_ref().is_none_or(|(parent, signal)| {
                !Arc::ptr_eq(parent, thread) || *signal != syscall_abi::signal::SIGCHLD
            })
        });
        let recipient = |(thread, signal)| PreparedChildRecipient {
            thread,
            signal,
            record: PreparedSignalRecord::default(),
        };
        Self {
            parent: parent.map(recipient),
            adoption: adoption.map(|thread| recipient((thread, syscall_abi::signal::SIGCHLD))),
        }
    }
    pub(in crate::execution::task::process_table) fn prepare(&mut self) {
        for recipient in [&mut self.parent, &mut self.adoption].into_iter().flatten() {
            if recipient.signal != 0 {
                recipient.record = recipient
                    .thread
                    .prepare_signal_record(recipient.signal, PendingSignal::child_exited(0, 0));
            }
        }
    }
    pub(in crate::execution::task::process_table) fn matches(
        &self,
        graph: &ProcessGraph,
        child: usize,
        scope: ChildNotificationScope,
    ) -> bool {
        let current = Self::capture(graph, child, scope);
        let same = |before: &Option<PreparedChildRecipient>,
                    after: &Option<PreparedChildRecipient>| match (before, after)
        {
            (None, None) => true,
            (Some(before), Some(after)) => {
                Arc::ptr_eq(&before.thread, &after.thread) && before.signal == after.signal
            }
            _ => false,
        };
        same(&self.parent, &current.parent) && same(&self.adoption, &current.adoption)
    }
    fn record_for(
        &mut self,
        recipient: &Arc<TaskControlBlock>,
        signal: usize,
    ) -> &mut PreparedSignalRecord {
        for prepared in [&mut self.parent, &mut self.adoption].into_iter().flatten() {
            if Arc::ptr_eq(&prepared.thread, recipient) && prepared.signal == signal {
                return &mut prepared.record;
            }
        }
        panic!("child notification recipient/signal changed after final graph recheck")
    }
}

/// native parent_exec_id 比較；父映像切换只改变 notification，不把 child 改成普通 wait class。
pub(in crate::execution::task::process_table) fn exit_notification_signal(
    graph: &ProcessGraph,
    child: &ProcessNode,
) -> usize {
    if child
        .parent
        .and_then(|parent| graph.nodes.get(&parent))
        .is_some_and(|parent| parent.exec_generation != child.parent_exec_generation)
    {
        syscall_abi::signal::SIGCHLD
    } else {
        usize::from(child.exit_signal)
    }
}

/// 返回 owned preparation 和最终 graph guard；caller 必须先声明 preparation，再声明 guard。
/// stale preparation/记录 OOM 的 token 始终在 graph 外退款；actual child Process 复用则停止。
pub(in crate::execution::task::process_table) fn lock_child_notifications(
    child: usize,
    scope: ChildNotificationScope,
) -> Option<(
    PreparedChildNotifications,
    crate::execution::sync::IrqMutexGuard<'static, ProcessGraph>,
)> {
    let mut graph = PROCESS_TABLE.graph.lock();
    let actual_child = graph.nodes.get(&child)?.process.clone();
    loop {
        let mut prepared = PreparedChildNotifications::capture(&graph, child, scope);
        drop(graph);
        prepared.prepare();
        graph = PROCESS_TABLE.graph.lock();
        if graph
            .nodes
            .get(&child)
            .is_none_or(|node| !Arc::ptr_eq(&node.process, &actual_child))
        {
            drop(graph);
            return None;
        }
        if prepared.matches(&graph, child, scope) {
            return Some((prepared, graph));
        }
        drop(graph);
        drop(prepared);
        graph = PROCESS_TABLE.graph.lock();
        if graph
            .nodes
            .get(&child)
            .is_none_or(|node| !Arc::ptr_eq(&node.process, &actual_child))
        {
            drop(graph);
            return None;
        }
    }
}

/// graph 中已发布的 child signal；只保活准确的调度 wake target，不保存 disposition。
pub(in crate::execution::task::process_table) struct ParentSignalWake {
    parent: usize,
    eligible: Option<Arc<TaskControlBlock>>,
}
impl ParentSignalWake {
    pub(in crate::execution::task::process_table) fn publish(self) {
        if !wake_process_signal_waiter(self.parent)
            && let Some(target) = self.eligible
            && !interrupt_waiting_task(&target)
        {
            crate::execution::task::processor::request_task_reschedule(&target);
        }
    }
}

/// Graph 以 child 的 canonical real-parent/reaper Thread 生成实际 exit signal 或 job SIGCHLD。
/// 其 blocked mask 决定 DFL-ignore 的 generation；任取 sibling 会错误丢弃 sigwait 通知。
pub(in crate::execution::task::process_table) fn stage_child_signal(
    graph: &ProcessGraph,
    child: usize,
    info: PendingSignal,
    kind: super::super::super::model::ChildSignalKind,
    prepared: &mut PreparedChildNotifications,
) -> Option<super::super::super::model::ChildSignalPublication> {
    let child = graph.nodes.get(&child)?;
    let parent = child.parent?;
    let creator = child.parent_thread?;
    let ProcessState::Live(threads) = &graph.nodes.get(&parent)?.state else {
        return None;
    };
    let recipient = threads.get(&creator)?;
    let uid = super::super::super::model::leader_signal_uid(&child.leader_credentials);
    let info = info.with_identity(
        child.pid.identity().number_in(&recipient.pid_namespace()),
        recipient.user_namespace().display_id(true, uid),
        false,
    );
    let signal = match kind {
        super::super::super::model::ChildSignalKind::Exit => exit_notification_signal(graph, child),
        super::super::super::model::ChildSignalKind::Job => syscall_abi::signal::SIGCHLD,
    };
    recipient.publish_child_signal(info, kind, signal, prepared.record_for(recipient, signal))
}

/// Pending publication 后、graph 解锁前只复制准确 target Arc；不得在此再生成信号。
pub(in crate::execution::task::process_table) fn prepare_parent_signal_wake(
    graph: &ProcessGraph,
    parent: usize,
    signal: usize,
) -> ParentSignalWake {
    let eligible = graph.nodes.get(&parent).and_then(|node| {
        let ProcessState::Live(threads) = &node.state else {
            return None;
        };
        threads
            .values()
            .find(|thread| {
                thread.accepts_process_signal(
                    signal,
                    super::super::ptrace::thread_traced(graph, thread.tid()),
                )
            })
            .cloned()
    });
    ParentSignalWake { parent, eligible }
}
