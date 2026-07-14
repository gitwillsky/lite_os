use alloc::sync::Arc;

use crate::{
    arch::hart,
    fallible_tree::FallibleMap,
    task::{
        PendingSignal, TaskControlBlock, WaitResult, current_task,
        processor::request_tick_reschedule,
    },
    timer::{get_time_ns, get_time_us},
};

use super::{
    ProcessState, TASK_MANAGER, load_average, process_terminal_input, send_kernel_process_signal,
    wait_key::IndexedWaitKind, wait_registry::INDEXED_WAIT_QUEUE, wake_console_waiters,
};

const TIMER_WORK_BATCH: usize = 32;
type ExpiredWait = (u64, Arc<TaskControlBlock>, IndexedWaitKind);

/// @description 一个 Process 的用户可见 ITIMER_REAL 状态。
#[derive(Clone, Copy)]
struct RealTimer {
    next_expiration_us: Option<u64>,
    interval_us: u64,
}

impl RealTimer {
    fn snapshot(self, now_us: u64) -> (u64, u64) {
        (
            self.next_expiration_us
                .map_or(0, |expiration| expiration.saturating_sub(now_us)),
            self.interval_us,
        )
    }
}

/// @description ITIMER_REAL record 与 active deadline index 的唯一复合状态 owner。
pub(super) struct RealTimerQueue {
    timers: FallibleMap<usize, RealTimer>,
    // OWNER: 仅本类型在同一 timer lock 下同步 record 与 active `(deadline, TGID)` membership。
    // 缺失 index 会让每个 tick 扫描全部 Process；分离写入口会漏发或重复 SIGALRM。
    deadline_index: FallibleMap<(u64, usize), ()>,
}

impl RealTimerQueue {
    /// @description 构造没有 timer record 或 deadline membership 的 owner。
    ///
    /// @return 空 ITIMER_REAL queue。
    pub(super) fn new() -> Self {
        Self {
            timers: FallibleMap::new(),
            deadline_index: FallibleMap::new(),
        }
    }

    fn take(&mut self, tgid: usize) -> Option<RealTimer> {
        let timer = self.timers.remove(&tgid)?;
        if let Some(expiration) = timer.next_expiration_us {
            assert!(self.deadline_index.remove(&(expiration, tgid)).is_some());
        }
        Some(timer)
    }

    /// @description 从 Process exit lifecycle 删除 timer record 与 active index membership。
    ///
    /// @param tgid 已确定完成最后一个 Thread exit 的 Process ID。
    /// @return 无返回值；没有 timer 时保持空操作。
    pub(super) fn remove(&mut self, tgid: usize) {
        self.take(tgid);
    }

    fn replace(
        &mut self,
        tgid: usize,
        value_us: u64,
        interval_us: u64,
        now_us: u64,
    ) -> Result<(u64, u64), RealTimerError> {
        let next_expiration_us = (value_us != 0).then(|| now_us.saturating_add(value_us));
        let replacement = (next_expiration_us.is_some() || interval_us != 0).then_some(RealTimer {
            next_expiration_us,
            interval_us,
        });
        let current = self.timers.get(&tgid).copied();

        // 1. 仅为当前事务缺少的 membership 预留节点；失败时旧 record/index 完全不变。
        let prepared_timer = if current.is_none() {
            replacement
                .map(|timer| FallibleMap::try_prepare(tgid, timer))
                .transpose()
                .map_err(|_| RealTimerError::OutOfMemory)?
        } else {
            None
        };
        let prepared_deadline = if current.and_then(|timer| timer.next_expiration_us).is_none() {
            next_expiration_us
                .map(|expiration| FallibleMap::try_prepare((expiration, tgid), ()))
                .transpose()
                .map_err(|_| RealTimerError::OutOfMemory)?
        } else {
            None
        };

        // 2. 预留完成后才摘下旧 index；active→active 直接复用同一个 AVL node。
        let previous = current.map_or((0, 0), |timer| timer.snapshot(now_us));
        let mut deadline = current.and_then(|timer| {
            timer.next_expiration_us.map(|expiration| {
                self.deadline_index
                    .take_entry(&(expiration, tgid))
                    .expect("real-timer record lost deadline index")
            })
        });
        match replacement {
            Some(timer) => {
                if let Some(current) = self.timers.get_mut(&tgid) {
                    *current = timer;
                } else {
                    self.timers
                        .commit_vacant(prepared_timer.expect("new timer node was not prepared"));
                }
            }
            None => {
                self.timers.remove(&tgid);
            }
        }
        // 3. active record 必须在同一锁内发布精确 key；提交阶段不再分配。
        if let Some(expiration) = next_expiration_us {
            let entry = if let Some(mut entry) = deadline.take() {
                entry.set_key((expiration, tgid));
                entry
            } else {
                prepared_deadline.expect("new deadline node was not prepared")
            };
            self.deadline_index.commit_vacant(entry);
        }
        Ok(previous)
    }

    fn current(&self, tgid: usize, now_us: u64) -> (u64, u64) {
        self.timers
            .get(&tgid)
            .copied()
            .map_or((0, 0), |timer| timer.snapshot(now_us))
    }

    fn pop_expired(&mut self, now_us: u64) -> Option<usize> {
        let (&(expiration, tgid), _) = self.deadline_index.first_key_value()?;
        if expiration > now_us {
            return None;
        }
        // 1. 从有序 index 摘下最早 deadline，并校验它仍指向同一 record。
        let mut deadline = self
            .deadline_index
            .take_entry(&(expiration, tgid))
            .expect("selected real-timer deadline disappeared");
        let timer = self
            .timers
            .get_mut(&tgid)
            .expect("real-timer deadline lost its record");
        assert_eq!(timer.next_expiration_us, Some(expiration));
        // 2. 周期 timer 沿原始相位跳过错过的周期；interval=0 通过 checked_div 直接 disarm。
        //    若从 handler 时刻重新计时，deferred 延迟会永久累积成周期漂移。
        timer.next_expiration_us = now_us
            .saturating_sub(expiration)
            .checked_div(timer.interval_us)
            .and_then(|elapsed_periods| {
                expiration.checked_add(
                    elapsed_periods
                        .saturating_add(1)
                        .saturating_mul(timer.interval_us),
                )
            });
        // 3. 只有仍 active 的 record 才重新发布 index membership。
        if let Some(next) = timer.next_expiration_us {
            deadline.set_key((next, tgid));
            self.deadline_index.commit_vacant(deadline);
        }
        Some(tgid)
    }

    fn has_expired(&self, now_us: u64) -> bool {
        self.deadline_index
            .first_key_value()
            .is_some_and(|(&(expiration, _), _)| expiration <= now_us)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RealTimerError {
    NotFound,
    OutOfMemory,
}

fn expire_real_timers(now_us: u64) {
    let mut targets = [None; TIMER_WORK_BATCH];
    // 1. 小 timer owner 锁内只摘取并重装固定 batch，不触碰 ProcessGraph 或分配 target Vec。
    let backlog = {
        let mut timers = TASK_MANAGER.real_timers.lock();
        for target in &mut targets {
            let Some(tgid) = timers.pop_expired(now_us) else {
                break;
            };
            *target = Some(tgid);
        }
        timers.has_expired(now_us)
    };
    // 2. signal seam 会获取 process graph，必须在释放 timer lock 后调用，避免 timer → graph 反向锁序。
    for tgid in targets.into_iter().flatten() {
        // ITIMER_REAL 由 kernel timer 产生；deferred/idle context 没有 userspace sender，
        // 若走 kill syscall 路径会因 current task 不存在而静默丢失 SIGALRM。
        let _ = send_kernel_process_signal(tgid, 14, PendingSignal::kernel());
    }
    // 3. 超出 batch 的到期项仅合并发布一个 bit；无界循环会饿死 I/O 与 user return。
    if backlog {
        hart::raise_timer_backlog_softirq();
    }
}

/// @description 原子替换 Process 的 ITIMER_REAL，并返回旧 timer 的剩余时间与 interval。
///
/// @param tgid 目标 live Process ID。
/// @param value_us 新 timer 首次到期前的微秒数；零表示 disarm。
/// @param interval_us 周期微秒数；零表示 one-shot。
/// @param now_us 本次替换固定的 monotonic 微秒时刻。
/// @return 旧 timer 的 `(remaining_us, interval_us)`。
/// @errors TGID 不存在或 Process 已不再 live 时返回 `Err(())`。
pub(crate) fn set_real_timer(
    tgid: usize,
    value_us: u64,
    interval_us: u64,
    now_us: u64,
) -> Result<(u64, u64), RealTimerError> {
    // graph → timer 是 set/get/exit 唯一锁序，保持 live TGID 校验到 timer mutation 原子。
    let graph = TASK_MANAGER.graph.lock();
    if !graph
        .nodes
        .get(&tgid)
        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
    {
        return Err(RealTimerError::NotFound);
    }
    let previous = TASK_MANAGER
        .real_timers
        .lock()
        .replace(tgid, value_us, interval_us, now_us)?;
    drop(graph);
    Ok(previous)
}

/// @description 查询 Process 当前 ITIMER_REAL 的剩余时间与 interval。
///
/// @param tgid 目标 live Process ID。
/// @param now_us 本次查询固定的 monotonic 微秒时刻。
/// @return 当前 timer 的 `(remaining_us, interval_us)`；未配置时返回 `(0, 0)`。
/// @errors TGID 不存在或 Process 已不再 live 时返回 `Err(())`。
pub(crate) fn real_timer(tgid: usize, now_us: u64) -> Result<(u64, u64), ()> {
    // 与 replace 共用 graph → timer 锁序，Process exit 不会在校验后留下 stale record。
    let graph = TASK_MANAGER.graph.lock();
    if !graph
        .nodes
        .get(&tgid)
        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
    {
        return Err(());
    }
    let current = TASK_MANAGER.real_timers.lock().current(tgid, now_us);
    drop(graph);
    Ok(current)
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
        expire_real_timers(now_us);
        request_tick_reschedule();
    } else if work & hart::TIMER_BACKLOG_SOFTIRQ != 0 {
        wake_expired_tasks(get_time_ns());
        expire_real_timers(get_time_us());
    }
    if work & hart::CONSOLE_SOFTIRQ != 0 {
        process_terminal_input();
        wake_console_waiters();
    }
    if work & hart::DISPLAY_SOFTIRQ != 0 {
        crate::drm::device::dispatch_display_work();
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
