use alloc::sync::Arc;

use crate::{
    cpu::{self, DeferredWork},
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
    wait_registry::WAIT_REGISTRY,
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
        cpu::raise_deferred(DeferredWork::TimerBacklog);
    }
}

/// @description 单锁摘取固定数量的到期 wait，锁外完成 wake，并为 backlog 发布续批。
///
/// @param now_ns 本批次固定的 absolute monotonic 纳秒时刻。
/// @return 无返回值；超过 batch 的已到期项由当前 CPU 的 timer backlog softirq 继续消费。
fn wake_expired_tasks(now_ns: u64) {
    let mut batch: [Option<ExpiredWait>; TIMER_WORK_BATCH] = core::array::from_fn(|_| None);
    // 1. 每次只锁 deadline 所在 source shard；跨 shard 的 registration 由 claim
    // 精确摘除，独立 deadline 不再争用单一全局 owner。
    for slot in &mut batch {
        let Some(expired) = WAIT_REGISTRY.expire_one(now_ns) else {
            break;
        };
        if let Some(claimed) = expired.claimed {
            *slot = Some((claimed.id, claimed.task, claimed.kind));
        }
    }
    let backlog = WAIT_REGISTRY.has_expired_deadline(now_ns);
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
        cpu::raise_deferred(DeferredWork::TimerBacklog);
    }
}

/// @description 仅在 user-return 或 local-IRQ-closed scheduler idle safe point 消费 deferred work。
///
/// kernel software-interrupt handler 不得调用本函数：它可重入持有普通 VirtIO queue、
/// DRM completion 或 KERNEL_SPACE lock 的 syscall；在该栈上消费会永久自旋。
pub(crate) fn dispatch_pending_deferred_work() {
    let work = cpu::take_deferred();
    if work.is_empty() {
        return;
    }
    if work.contains(DeferredWork::Timer) {
        let now_us = get_time_us();
        if let Some(task) = current_task() {
            task.scheduling.policy.lock().checkpoint_runtime(now_us);
        }
        wake_expired_tasks(get_time_ns());
        load_average::update(now_us);
        expire_timers(get_time_ns());
        request_tick_reschedule();
    } else if work.contains(DeferredWork::TimerBacklog) {
        wake_expired_tasks(get_time_ns());
        expire_timers(get_time_ns());
    }
    if work.contains(DeferredWork::Console) {
        let input_backlog = process_terminal_input();
        let waiter_backlog = wake_console_waiters();
        if input_backlog || waiter_backlog {
            cpu::raise_deferred(DeferredWork::Console);
        }
    }
    if work.contains(DeferredWork::Display) {
        crate::drm::device::dispatch_display_work(get_time_ns());
    }
    if work.contains(DeferredWork::Input) && crate::input::dispatch_input_work() {
        cpu::raise_deferred(DeferredWork::Input);
    }
    if work.contains(DeferredWork::DriverIo) && crate::drivers::dispatch_io_completion_work() {
        cpu::raise_deferred(DeferredWork::DriverIo);
    }
    let network_due = work.contains(DeferredWork::Network)
        || work.contains(DeferredWork::Timer) && crate::socket::network_work_due();
    if network_due {
        // RX budget 用尽时必须再次发布同一 deferred work；否则 used ring 中没有新 IRQ edge
        // 的 frame 可能永久滞留。timer deadline 同样在此推进 ARP/UDP egress；缺失时丢失
        // 首个 ARP reply 后将永远不重试。requeue 由 task deferred owner 执行，socket 不反向依赖 arch。
        if crate::socket::dispatch_network_work() {
            cpu::raise_deferred(DeferredWork::Network);
        }
    }
}
