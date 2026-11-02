//! ptrace 的 stopped/exit 与 SIGCHLD 原子发表；通知先于 wait 唤醒，准备与退休在图锁外。
use super::*;
use crate::task::model::{ChildSignalKind, PreparedSignalRecord};

/// 一次通知准备捕获的真实 attachment 与 tracer；不缓存 disposition 或镜像 tracing phase。
struct Notification {
    pid: Arc<PidRegistration>,
    group: usize,
    tracer: Arc<TaskControlBlock>,
    record: PreparedSignalRecord,
}
impl Notification {
    fn prepare(tid: usize) -> Option<Self> {
        let (pid, group, tracer) = {
            let graph = PROCESS_TABLE.graph.lock();
            let tracee = graph.tracees.get(&tid)?;
            (
                tracee.pid.clone(),
                tracee.tracer_group,
                thread_by_tid(&graph, tracee.tracer)?.1,
            )
        };
        let record = tracer.prepare_signal_record(
            linux_abi::signal::SIGCHLD,
            PendingSignal::child_exited(0, 0),
        );
        Some(Self {
            pid,
            group,
            tracer,
            record,
        })
    }
    fn matches(&self, graph: &ProcessGraph, tid: usize) -> bool {
        graph.tracees.get(&tid).is_some_and(|tracee| {
            Arc::ptr_eq(&tracee.pid, &self.pid)
                && tracee.tracer_group == self.group
                && tracee.tracer == self.tracer.tid()
                && thread_by_tid(graph, tracee.tracer)
                    .is_some_and(|(_, actual)| Arc::ptr_eq(&actual, &self.tracer))
        })
    }
    fn exit_info(&self, tracee: &Tracee, status: i32) -> PendingSignal {
        let pid = tracee
            .pid
            .identity()
            .number_in(&self.tracer.pid_namespace());
        let uid = self.tracer.user_namespace().display_id(true, tracee.uid);
        let info = if status & 0x7f == 0 {
            PendingSignal::child_exited(pid as usize, status >> 8)
        } else {
            PendingSignal::child_killed(pid as usize, (status & 0x7f) as usize)
        };
        info.with_identity(pid, uid, false)
    }
    fn stage(
        &mut self,
        graph: &ProcessGraph,
        info: PendingSignal,
        kind: ChildSignalKind,
    ) -> Option<super::super::signal::ParentSignalWake> {
        self.tracer
            .publish_child_signal(info, kind, linux_abi::signal::SIGCHLD, &mut self.record)
            .is_some_and(|publication| publication.queued)
            .then(|| {
                super::super::signal::prepare_parent_signal_wake(
                    graph,
                    self.group,
                    linux_abi::signal::SIGCHLD,
                )
            })
    }
    fn take_waiters(&self, graph: &mut ProcessGraph) -> FallibleMap<usize, Arc<TaskControlBlock>> {
        graph
            .nodes
            .get_mut(&self.group)
            .map(take_child_waiters)
            .unwrap_or_default()
    }
    fn finish(
        self,
        mut waiters: FallibleMap<usize, Arc<TaskControlBlock>>,
        wake: Option<super::super::signal::ParentSignalWake>,
    ) {
        // 未用 preparation 必须先退款；不能把已忽略通知的临时收费带到被唤醒 tracer 的下一次操作。
        drop(self);
        if let Some(wake) = wake {
            wake.publish();
        }
        while let Some((&tid, _)) = waiters.first_key_value() {
            let task = waiters.remove(&tid).expect("indexed tracer waiter");
            crate::task::processor::wake_child_task(task, WaitResult::Woken);
        }
    }
}

/// 真实停止安全点：先准备节点，再同锁发表 stopped/CLD_TRAPPED，最后在图外唤醒。
pub(in crate::task::process_table) fn complete_stop(task: &TaskControlBlock) {
    loop {
        let Some(mut notice) = Notification::prepare(task.tid()) else {
            return;
        };
        let mut graph = PROCESS_TABLE.graph.lock();
        if !notice.matches(&graph, task.tid()) {
            continue;
        }
        let tracee = graph
            .tracees
            .get_mut(&task.tid())
            .expect("validated tracing relation");
        let Phase::Stopping { signal, info } = tracee.phase else {
            return;
        };
        tracee.uid = task.credential_id(true, false);
        tracee.phase = Phase::Stopped {
            signal,
            info_signal: signal,
            info,
            reported: false,
        };
        let pid = task
            .tid_identity()
            .number_in(&notice.tracer.pid_namespace());
        let uid = notice.tracer.user_namespace().display_id(true, tracee.uid);
        let info = PendingSignal::ptrace_stopped(pid, uid, signal);
        let wake = notice.stage(&graph, info, ChildSignalKind::Job);
        let waiters = notice.take_waiters(&mut graph);
        drop(graph);
        notice.finish(waiters, wake);
        return;
    }
}

/// Thread 清理完成；leader 的可等待 exit 仍由最终 Process retirement 发表。
pub(super) fn publish_thread_exit(task: &TaskControlBlock, status: i32) {
    if task.tid() == task.tgid() {
        let mut graph = PROCESS_TABLE.graph.lock();
        if let Some(tracee) = graph.tracees.get_mut(&task.tid()) {
            tracee.uid = task.credential_id(true, false);
            tracee.phase = Phase::Exited(status);
        }
        return;
    }
    loop {
        let Some(mut notice) = Notification::prepare(task.tid()) else {
            return;
        };
        let mut graph = PROCESS_TABLE.graph.lock();
        if !notice.matches(&graph, task.tid()) {
            continue;
        }
        let tracee = graph
            .tracees
            .get_mut(&task.tid())
            .expect("validated tracing relation");
        tracee.uid = task.credential_id(true, false);
        tracee.phase = Phase::Exited(status);
        let info = notice.exit_info(tracee, status);
        let wake = notice.stage(&graph, info, ChildSignalKind::Exit);
        let waiters = notice.take_waiters(&mut graph);
        drop(graph);
        notice.finish(waiters, wake);
        return;
    }
}

/// Process 真实 retirement 完成后，把 exit 与通知一同交给 tracer；无 attachment 返回 false。
pub(in crate::task::process_table) fn defer_process_exit(tgid: usize) -> bool {
    loop {
        let Some(mut notice) = Notification::prepare(tgid) else {
            return false;
        };
        let mut graph = PROCESS_TABLE.graph.lock();
        if !notice.matches(&graph, tgid) {
            continue;
        }
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("traced process remains indexed");
        let ProcessState::Exited(status, ExitRetirement::Retiring) = node.state else {
            panic!("ptrace exit precedes retirement");
        };
        node.state = ProcessState::Exited(status, ExitRetirement::Traced);
        let source = node.pidfd_source.upgrade();
        let info = notice.exit_info(&graph.tracees[&tgid], status.wait_status());
        let wake = notice.stage(&graph, info, ChildSignalKind::Exit);
        let waiters = notice.take_waiters(&mut graph);
        drop(graph);
        notice.finish(waiters, wake);
        if let Some(source) = source {
            source.notify();
        }
        return true;
    }
}
