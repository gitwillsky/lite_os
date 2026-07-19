use super::*;

pub(super) struct PendingHandoff {
    task: Arc<TaskControlBlock>,
    irq: LocalIrqTransfer,
}

/// @description 把已经停止执行、尚未完成 scheduling consequence 的 outgoing owner
/// 发布到当前 CPU；next task 或 idle continuation 唯一消费。
/// @param task context save target 的唯一 outgoing owner。
/// @param irq 进入 scheduler transaction 前的 local IRQ restore consequence。
/// @return 无返回值。
/// @panics 当前 CPU 已有未消费 handoff 时 fail-stop。
pub(in crate::task) fn publish_pending_handoff(task: Arc<TaskControlBlock>, irq: LocalIrqGuard) {
    with_current_processor(|processor| {
        assert!(
            processor.pending_handoff.is_none(),
            "scheduler handoff consequence published twice"
        );
        processor.pending_handoff = Some(PendingHandoff {
            task,
            irq: irq.into_transfer(),
        });
    });
}

/// @description 消费前一次 kernel context switch 留下的 outgoing owner 与 IRQ guard。
/// @return 没有 task→task/task→idle handoff 时返回 None。
/// @panics 无。
pub(in crate::task) fn take_pending_handoff() -> Option<(Arc<TaskControlBlock>, LocalIrqTransfer)> {
    with_current_processor(|processor| {
        processor
            .pending_handoff
            .take()
            .map(|pending| (pending.task, pending.irq))
    })
}

/// @description runqueue 没有 successor 时，取消可继续执行 task 的过渡态，避免自我 yield
/// 仍进入 idle；真正 Blocking/Stopped 或 affinity 禁止当前 CPU 时返回 false。
/// @param task 已撤销 Processor current、尚未发布 post-switch consequence 的 outgoing owner。
/// @return 当前 logical CPU 可继续执行该 task 时返回 true。
/// @panics current/load/state owner 不一致时 fail-stop。
pub(in crate::task) fn resume_without_switch(task: &Arc<TaskControlBlock>) -> bool {
    let cpu = cpu::current_id();
    with_current_processor(|processor| {
        assert!(
            processor.current.is_none(),
            "resume-without-switch found a current owner"
        );
        let mut scheduling = task.scheduling.state.lock();
        let resumable = match scheduling.run_state() {
            RunState::Preempting { cpu: owner } | RunState::WakePending { cpu: owner } => {
                assert_eq!(owner, cpu, "handoff resumed on a different CPU");
                scheduling.cpu_affinity.allows(cpu)
            }
            _ => false,
        };
        if !resumable {
            return false;
        }
        scheduling.replace_non_ready_state(RunState::Running { cpu });
        drop(scheduling);
        processor.current = Some(task.clone());
        let previous = current_per_cpu()
            .running_entries
            .fetch_add(1, Ordering::Relaxed);
        assert_eq!(previous, 0, "running load already owned during self resume");
        true
    })
}
