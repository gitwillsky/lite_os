use super::*;

/// @description 撤销 Running membership 并进入 preemption 或既有 group-stop 交接状态。
///
/// @param task 当前 CPU 唯一 running Task。
/// @return 无返回值；Ready/Stopped 发布必须等源 CPU 切回 idle stack。
pub(in crate::task) fn begin_preempt_running_task(task: &Arc<TaskControlBlock>) {
    let source_cpu = cpu::current_id();
    let current =
        with_current_processor(Processor::take_current).expect("preemption requires current task");
    assert!(Arc::ptr_eq(&current, task));
    let mut scheduling = task.scheduling.state.lock();
    match scheduling.run_state() {
        RunState::Running { cpu } => {
            assert_eq!(cpu, source_cpu, "preemption source CPU diverged");
            scheduling.replace_non_ready_state(RunState::Preempting { cpu: source_cpu });
        }
        RunState::StopPending {
            cpu,
            transition: StopTransition::Running,
        } => assert_eq!(cpu, source_cpu, "stop source CPU diverged"),
        state => panic!("preemption source lost running ownership: {state:?}"),
    }
}

pub(super) fn request_reschedule_on(cpu: CpuId) {
    publish_reschedule_at(cpu);
}

/// @description timer tick 到期时仅在当前 CPU 存在 runnable 竞争者才请求抢占。
///
/// @return 无返回值；单一 Running task 不产生无意义的自我 context switch。
pub(in crate::task) fn request_tick_reschedule() {
    let slot = current_per_cpu();
    let competitors = slot.ready_entries.load(Ordering::Relaxed);
    if slot.running_entries.load(Ordering::Relaxed) != 0 && competitors != 0 {
        request_reschedule();
    }
}

/// @description 请求正在运行或过渡中的目标 Thread 尽快进入 kernel 调度点。
///
/// @param task Process graph 持有的目标 Thread。
/// @return 无返回值；非 CPU-owned 状态已会自然进入 trap return，不发送冗余 IPI。
pub(in crate::task) fn request_task_reschedule(task: &Arc<TaskControlBlock>) {
    let cpu = match task.scheduling.state.lock().run_state() {
        RunState::Running { cpu }
        | RunState::Preempting { cpu }
        | RunState::Blocking { cpu }
        | RunState::WakePending { cpu }
        | RunState::StopPending { cpu, .. } => Some(cpu),
        RunState::New
        | RunState::Ready { .. }
        | RunState::Blocked
        | RunState::Stopped { .. }
        | RunState::Exited => None,
    };
    if let Some(cpu) = cpu {
        request_reschedule_on(cpu);
    }
}

/// @description 将一个 live Thread 的 scheduler membership 转为 group-stop pending/stopped。
///
/// @param task Process graph 持有的目标 Thread。
/// @return 无返回值；Running/transitioning Thread 会收到 reschedule IPI。
pub(in crate::task) fn request_task_stop(task: &Arc<TaskControlBlock>) {
    let reschedule_cpu = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::New => {
                scheduling.replace_non_ready_state(RunState::Stopped {
                    resume: StopResume::Runnable,
                });
                None
            }
            RunState::Ready { cpu, .. } => {
                commit_ready_retirement(scheduling.transition_ready_to_stopped());
                Some(cpu)
            }
            RunState::Running { cpu } => {
                scheduling.replace_non_ready_state(RunState::StopPending {
                    cpu,
                    transition: StopTransition::Running,
                });
                Some(cpu)
            }
            RunState::Preempting { cpu } => {
                scheduling.replace_non_ready_state(RunState::StopPending {
                    cpu,
                    transition: StopTransition::Preempting,
                });
                Some(cpu)
            }
            RunState::Blocking { cpu } => {
                scheduling.replace_non_ready_state(RunState::StopPending {
                    cpu,
                    transition: StopTransition::Blocking,
                });
                Some(cpu)
            }
            RunState::Blocked => {
                scheduling.replace_non_ready_state(RunState::Stopped {
                    resume: StopResume::Blocked,
                });
                None
            }
            RunState::WakePending { cpu } => {
                scheduling.replace_non_ready_state(RunState::StopPending {
                    cpu,
                    transition: StopTransition::WakePending,
                });
                Some(cpu)
            }
            RunState::StopPending { .. } | RunState::Stopped { .. } | RunState::Exited => None,
        }
    };
    if let Some(cpu) = reschedule_cpu {
        request_reschedule_on(cpu);
    }
}

/// @description 取消 group stop 并恢复 Thread 原有 runnable/blocked transition。
///
/// @param task Process graph 持有的目标 Thread。
/// @return 无返回值；恢复为 Ready 时函数完成唯一 enqueue。
pub(in crate::task) fn continue_stopped_task(task: Arc<TaskControlBlock>) {
    let ready = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::Stopped {
                resume: StopResume::Runnable,
            } => {
                let cpu = select_cpu(&task, scheduling.cpu_affinity);
                let generation = commit_ready_transition(scheduling.transition_to_ready(cpu));
                Some((cpu, generation))
            }
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => {
                scheduling.replace_non_ready_state(RunState::Blocked);
                None
            }
            RunState::StopPending { cpu, transition } => {
                scheduling.replace_non_ready_state(match transition {
                    StopTransition::Running => RunState::Running { cpu },
                    StopTransition::Preempting => RunState::Preempting { cpu },
                    StopTransition::Blocking => RunState::Blocking { cpu },
                    StopTransition::WakePending => RunState::WakePending { cpu },
                });
                None
            }
            _ => None,
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(task, generation));
    }
}
