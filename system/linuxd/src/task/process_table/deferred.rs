use crate::sync::WaitResult;
use alloc::sync::Arc;

use crate::{
    deferred::{self, DeferredWork, DeferredWorkSet},
    task::{PendingSignal, TaskControlBlock},
    time::get_time_ns,
};

use super::{
    PROCESS_TABLE, load_average, process_terminal_input, send_kernel_process_signal,
    timer_queue::ExpiredTimer, wait_key::IndexedWaitKind, wait_registry::WAIT_REGISTRY,
    wake_console_waiters,
};

const TIMER_WORK_BATCH: usize = 32;
type ExpiredWait = (u64, Arc<TaskControlBlock>, IndexedWaitKind);

fn expire_timers(now_ns: u64) {
    let mut targets: [Option<ExpiredTimer>; TIMER_WORK_BATCH] = core::array::from_fn(|_| None);
    // 1. timer owner 锁内只摘取并重装固定 batch，不触碰 ProcessGraph 或分配 target Vec。
    let backlog = {
        let mut timers = PROCESS_TABLE.timers.lock();
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
            ExpiredTimer::Real { tgid, retired } => {
                drop(retired);
                let _ = send_kernel_process_signal(
                    tgid,
                    syscall_abi::signal::SIGALRM,
                    PendingSignal::kernel(),
                );
            }
            ExpiredTimer::Posix(timer) => super::signal::publish_posix_timer_signal(timer),
            ExpiredTimer::File {
                target,
                elapsed,
                generation,
                retired,
            } => {
                drop(retired);
                target.expire(elapsed, generation);
            }
            ExpiredTimer::Kernel(expired) => expired.complete(),
            ExpiredTimer::Silent(retired) => drop(retired),
        }
    }
    // 3. 超出 batch 的到期项仅合并发布一个 bit；无界循环会饿死 I/O 与 user return。
    if backlog {
        deferred::raise(DeferredWork::TIMER_BACKLOG);
    }
}

/// 单锁摘取固定数量的到期 wait，锁外完成 wake，并为 backlog 发布续批。
///
/// # Parameters
///
/// - `now_ns`: 本批次固定的 absolute monotonic 纳秒时刻。
///
/// # Returns
///
/// 无返回值；超过 batch 的已到期项由当前 CPU 的 timer backlog softirq 继续消费。
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
            IndexedWaitKind::TaskEvent(key) => crate::task::processor::wake_waiting_task(
                task,
                super::WaitMembership::TaskWait(key),
                Some(WaitResult::TimedOut),
            ),
            IndexedWaitKind::Deadline => {
                crate::task::processor::wake_deadline_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Futex { .. } => {
                crate::task::processor::wake_futex_task(task, wait_id, WaitResult::TimedOut)
            }
            IndexedWaitKind::Signal { .. } => {
                crate::task::processor::wake_signal_task(task, wait_id, WaitResult::TimedOut)
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
        deferred::raise(DeferredWork::TIMER_BACKLOG);
    }
}

/// deferred 线程的分派：定时器到期、控制台输入与已注册设备工作。
pub(crate) fn dispatch_deferred_work(work: DeferredWorkSet) {
    if work.contains(DeferredWork::TIMER) {
        wake_expired_tasks(get_time_ns());
        load_average::update(crate::time::get_time_us());
        expire_timers(get_time_ns());
        super::resource_limit::sample_cpu_limits(get_time_ns());
    } else if work.contains(DeferredWork::TIMER_BACKLOG) {
        wake_expired_tasks(get_time_ns());
        expire_timers(get_time_ns());
    }
    if work.contains(DeferredWork::CONSOLE) {
        let input_backlog = process_terminal_input();
        let waiter_backlog = wake_console_waiters();
        if input_backlog || waiter_backlog {
            deferred::raise(DeferredWork::CONSOLE);
        }
    }
    work.run_registered(get_time_ns());
}

/// 全部定时器来源中最早的截止时间：等待截止、间隔/POSIX/timerfd 定时器、负载均值采样与 RLIMIT_CPU 采样。
pub(crate) fn next_deferred_deadline() -> Option<u64> {
    let waits = WAIT_REGISTRY.next_deadline();
    let timers = PROCESS_TABLE.timers.lock().next_expiration();
    let load = PROCESS_TABLE
        .load_average
        .next_sample_us()
        .map(|deadline| deadline.saturating_mul(1000));
    let cpu_limits = super::resource_limit::cpu_limit_sample_deadline(get_time_ns());
    [waits, timers, load, cpu_limits]
        .into_iter()
        .flatten()
        .min()
}
