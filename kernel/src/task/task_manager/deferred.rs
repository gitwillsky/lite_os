use alloc::sync::Arc;

use crate::{
    arch::hart,
    task::{
        PendingSignal, TaskControlBlock, WaitResult, current_task,
        processor::request_tick_reschedule,
    },
    timer::{get_time_ns, get_time_us},
};

use super::{
    TASK_MANAGER, load_average, process_terminal_input, send_kernel_process_signal,
    send_kernel_thread_signal_info,
    timer_queue::{ExpiredTimer, PosixTimerNotification},
    wait_key::IndexedWaitKind,
    wait_registry::INDEXED_WAIT_QUEUE,
    wake_console_waiters,
};

const TIMER_WORK_BATCH: usize = 32;
type ExpiredWait = (u64, Arc<TaskControlBlock>, IndexedWaitKind);

fn expire_timers(now_ns: u64) {
    let mut targets = [None; TIMER_WORK_BATCH];
    // 1. timer owner 锁内只摘取并重装固定 batch，不触碰 ProcessGraph 或分配 target Vec。
    let backlog = {
        let mut timers = TASK_MANAGER.timers.lock();
        for target in &mut targets {
            let Some(expired) = timers.pop_expired(now_ns) else {
                break;
            };
            *target = Some(expired);
        }
        timers.has_expired(now_ns)
    };
    // 2. signal seam 会获取 process graph，必须在释放 timer lock 后调用，避免 timer → graph 反向锁序。
    for expired in targets.into_iter().flatten() {
        match expired {
            ExpiredTimer::Real(tgid) => {
                let _ = send_kernel_process_signal(tgid, 14, PendingSignal::kernel());
            }
            ExpiredTimer::Posix(timer) => {
                let info = PendingSignal::timer(
                    timer.id,
                    timer.overrun,
                    match timer.notification {
                        PosixTimerNotification::Process { value, .. }
                        | PosixTimerNotification::Thread { value, .. } => value,
                        PosixTimerNotification::Default | PosixTimerNotification::None => 0,
                    },
                );
                match timer.notification {
                    PosixTimerNotification::Process { signal, .. } => {
                        let _ = send_kernel_process_signal(timer.tgid, signal, info);
                    }
                    PosixTimerNotification::Thread { tid, signal, .. } => {
                        let _ = send_kernel_thread_signal_info(timer.tgid, tid, signal, info);
                    }
                    PosixTimerNotification::Default | PosixTimerNotification::None => {
                        unreachable!("timer owner normalizes silent/default notifications")
                    }
                }
            }
            ExpiredTimer::Silent => {}
        }
    }
    // 3. 超出 batch 的到期项仅合并发布一个 bit；无界循环会饿死 I/O 与 user return。
    if backlog {
        hart::raise_timer_backlog_softirq();
    }
}

/// @description 单锁摘取固定数量的到期 wait，锁外完成 wake，并为 backlog 发布续批。
///
/// @param now_ns 本批次固定的 absolute monotonic 纳秒时刻。
/// @return 无返回值；超过 batch 的已到期项由当前 hart 的 timer backlog softirq 继续消费。
fn wake_expired_tasks(now_ns: u64) {
    let mut batch: [Option<ExpiredWait>; TIMER_WORK_BATCH] = core::array::from_fn(|_| None);
    // 1. 一次 registry owner lock 摘取完整 batch，避免每个 waiter 重复关中断和抢锁。
    let backlog = {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        for slot in &mut batch {
            let Some(expired) = queue.pop_expired(now_ns) else {
                break;
            };
            *slot = Some(expired);
        }
        queue.has_expired_deadline(now_ns)
    };
    // 2. wake 会获取 scheduling/runqueue owner，必须在释放 registry lock 后执行。
    for (wait_id, task, kind) in batch.into_iter().flatten() {
        let woke = match kind {
            IndexedWaitKind::Deadline => {
                crate::task::processor::wake_deadline_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Futex { .. } => {
                crate::task::processor::wake_futex_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Signal { .. } => {
                crate::task::processor::wake_signal_task(task, WaitResult::TimedOut)
            }
            IndexedWaitKind::Console => {
                crate::task::processor::wake_console_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(task, wait_id, WaitResult::TimedOut)
            }
            _ => panic!("non-deadline wait carried a deadline"),
        };
        assert!(woke, "expired wait diverged from scheduling membership");
    }
    // 3. backlog 只发布一个合并 bit；直接无界循环会让 I/O 与 user return 永久饥饿。
    if backlog {
        hart::raise_timer_backlog_softirq();
    }
}

/// @description 在 user-return 或 scheduler idle context 消费全部 deferred work。
pub(crate) fn dispatch_pending_deferred_work() {
    let work = hart::take_softirqs();
    if work == 0 {
        return;
    }
    if work & hart::TIMER_SOFTIRQ != 0 {
        let now_us = get_time_us();
        if let Some(task) = current_task() {
            task.scheduling.policy.lock().checkpoint_runtime(now_us);
        }
        wake_expired_tasks(get_time_ns());
        load_average::update(now_us);
        expire_timers(get_time_ns());
        request_tick_reschedule();
    } else if work & hart::TIMER_BACKLOG_SOFTIRQ != 0 {
        wake_expired_tasks(get_time_ns());
        expire_timers(get_time_ns());
    }
    if work & hart::CONSOLE_SOFTIRQ != 0 {
        process_terminal_input();
        wake_console_waiters();
    }
    if work & hart::DISPLAY_SOFTIRQ != 0 {
        crate::drm::device::dispatch_display_work(get_time_ns());
    }
    if work & hart::INPUT_SOFTIRQ != 0 && crate::input::dispatch_input_work() {
        hart::raise_input_softirq();
    }
    let network_due = work & hart::NETWORK_SOFTIRQ != 0
        || work & hart::TIMER_SOFTIRQ != 0 && crate::socket::network_work_due();
    if network_due {
        // RX budget 用尽时必须再次发布同一 deferred work；否则 used ring 中没有新 IRQ edge
        // 的 frame 可能永久滞留。timer deadline 同样在此推进 ARP/UDP egress；缺失时丢失
        // 首个 ARP reply 后将永远不重试。requeue 由 task deferred owner 执行，socket 不反向依赖 arch。
        if crate::socket::dispatch_network_work() {
            hart::raise_network_softirq();
        }
    }
}
