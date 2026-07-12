use super::*;

/// @description 撤销 Running membership 并进入 preemption 或既有 group-stop 交接状态。
///
/// @param task 当前 hart 唯一 running Task。
/// @return 无返回值；Ready/Stopped 发布必须等源 hart 切回 idle stack。
pub(in crate::task) fn begin_preempt_running_task(task: &Arc<TaskControlBlock>) {
    let source_cpu = hart_id();
    let current =
        with_current_processor(Processor::take_current).expect("preemption requires current task");
    assert!(Arc::ptr_eq(&current, task));
    let mut scheduling = task.scheduling.state.lock();
    match scheduling.run_state {
        RunState::Running { cpu } => {
            assert_eq!(cpu, source_cpu, "preemption source CPU diverged");
            scheduling.run_state = RunState::Preempting { cpu: source_cpu };
        }
        RunState::StopPending {
            cpu,
            transition: StopTransition::Running,
        } => assert_eq!(cpu, source_cpu, "stop source CPU diverged"),
        state => panic!("preemption source lost running ownership: {state:?}"),
    }
}

fn request_reschedule_on(cpu: usize) {
    per_hart(cpu)
        .reschedule_requested
        .store(true, Ordering::Release);
    if cpu != hart_id() {
        sbi::sbi_send_ipi(1usize << cpu, 0).expect("SBI IPI failed for remote reschedule");
    }
}

/// @description 将一个 live Thread 的 scheduler membership 转为 group-stop pending/stopped。
///
/// @param task Process graph 持有的目标 Thread。
/// @return 无返回值；Running/transitioning Thread 会收到 reschedule IPI。
pub(in crate::task) fn request_task_stop(task: &Arc<TaskControlBlock>) {
    let reschedule_cpu = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state {
            RunState::New => {
                scheduling.run_state = RunState::Stopped {
                    resume: StopResume::Runnable,
                };
                None
            }
            RunState::Ready { cpu, .. } => {
                scheduling.run_state = RunState::Stopped {
                    resume: StopResume::Runnable,
                };
                Some(cpu)
            }
            RunState::Running { cpu } => {
                scheduling.run_state = RunState::StopPending {
                    cpu,
                    transition: StopTransition::Running,
                };
                Some(cpu)
            }
            RunState::Preempting { cpu } => {
                scheduling.run_state = RunState::StopPending {
                    cpu,
                    transition: StopTransition::Preempting,
                };
                Some(cpu)
            }
            RunState::Blocking { cpu } => {
                scheduling.run_state = RunState::StopPending {
                    cpu,
                    transition: StopTransition::Blocking,
                };
                Some(cpu)
            }
            RunState::Blocked => {
                scheduling.run_state = RunState::Stopped {
                    resume: StopResume::Blocked,
                };
                None
            }
            RunState::WakePending { cpu } => {
                scheduling.run_state = RunState::StopPending {
                    cpu,
                    transition: StopTransition::WakePending,
                };
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
        match scheduling.run_state {
            RunState::Stopped {
                resume: StopResume::Runnable,
            } => {
                let cpu = select_cpu(&task);
                Some((cpu, scheduling.transition_to_ready(cpu)))
            }
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => {
                scheduling.run_state = RunState::Blocked;
                None
            }
            RunState::StopPending { cpu, transition } => {
                scheduling.run_state = match transition {
                    StopTransition::Running => RunState::Running { cpu },
                    StopTransition::Preempting => RunState::Preempting { cpu },
                    StopTransition::Blocking => RunState::Blocking { cpu },
                    StopTransition::WakePending => RunState::WakePending { cpu },
                };
                None
            }
            _ => None,
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(task, generation));
    }
}
