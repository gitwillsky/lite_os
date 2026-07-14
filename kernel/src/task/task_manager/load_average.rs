use core::sync::atomic::{AtomicU64, Ordering};

use crate::{sync::IrqMutex, task::RunState};

use super::{ProcessState, TASK_MANAGER};

const FIXED_ONE: u64 = 1 << 11;
const INTERVAL_US: u64 = 5_000_000;
const EXP: [u64; 3] = [1884, 2014, 2037];

/// @description 已取得且尚未提交的连续 load-average 采样周期。
struct SampleClaim {
    periods: u64,
    next_deadline_us: u64,
}

/// @description Linux 1/5/15 minute fixed-point load average 的唯一 cadence 与 value owner。
pub(super) struct LoadAverage {
    // OWNER: 本 atomic 唯一分配 5-second sample cadence；0 是 in-progress sentinel。
    // 缺失 claim 会让每个 hart 重复扫描全部 Task；缺失 sentinel 会允许后一周期先提交，
    // 使不同 active sample 以反向时间顺序更新 EWMA。
    sample_deadline_us: AtomicU64,
    // OWNER: 本锁原子发布同一次 sample 的三个 EWMA value；拆锁会让 procfs/sysinfo
    // 观察到跨 sample 的撕裂组合。
    fixed: IrqMutex<[u64; 3]>,
}

impl LoadAverage {
    /// @description 构造从 monotonic 5-second deadline 开始的空 EWMA owner。
    ///
    /// @return 三个 load value 均为零且没有 in-progress sample 的 owner。
    /// @errors 无错误。
    pub(super) fn new() -> Self {
        Self {
            sample_deadline_us: AtomicU64::new(INTERVAL_US),
            fixed: IrqMutex::new([0; 3]),
        }
    }

    fn claim(&self, now_us: u64) -> Option<SampleClaim> {
        loop {
            // 1. 零值表示前一 claimant 尚未提交；继续 claim 会让 EWMA update 时间顺序反转。
            let deadline = self.sample_deadline_us.load(Ordering::Relaxed);
            if deadline == 0 || now_us < deadline {
                return None;
            }
            // 2. 一次 claim 合并所有已错过周期，next 始终严格晚于本次固定 now。
            let periods = (now_us - deadline)
                .checked_div(INTERVAL_US)
                .and_then(|periods| periods.checked_add(1))
                .expect("load-average sample period exhausted");
            let next_deadline_us = periods
                .checked_mul(INTERVAL_US)
                .and_then(|advance| deadline.checked_add(advance))
                .expect("load-average deadline exhausted monotonic time");
            // 3. Atomic 只分配 cadence，不发布 fixed values；value visibility 由 fixed lock 保证。
            if self
                .sample_deadline_us
                .compare_exchange_weak(deadline, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Some(SampleClaim {
                    periods,
                    next_deadline_us,
                });
            }
        }
    }

    fn commit(&self, claim: SampleClaim, runnable: usize) {
        let active = u64::try_from(runnable)
            .expect("runnable task count exceeds RV64")
            .checked_mul(FIXED_ONE)
            .expect("load-average active value overflow");
        {
            let mut fixed = self.fixed.lock();
            for (load, exp) in fixed.iter_mut().zip(EXP) {
                *load = calc_load(*load, fixed_power(exp, claim.periods), active);
            }
        }
        self.sample_deadline_us
            .store(claim.next_deadline_us, Ordering::Relaxed);
    }

    /// @description 投影当前 1/5/15 minute load average 为千分制数值。
    ///
    /// @return 同一次 committed sample 的三个 load values。
    /// @errors 内部 fixed-point value 无法表达为 u64 时 fail-stop。
    pub(super) fn values(&self) -> [u64; 3] {
        self.fixed.lock().map(|load| {
            u64::try_from((load as u128) * 1_000 / FIXED_ONE as u128)
                .expect("load-average projection overflow")
        })
    }
}

fn fixed_multiply(left: u64, right: u64) -> u64 {
    debug_assert!(left <= FIXED_ONE && right <= FIXED_ONE);
    (left * right + FIXED_ONE / 2) / FIXED_ONE
}

fn fixed_power(mut base: u64, mut exponent: u64) -> u64 {
    // 1. FIXED_ONE 是乘法单位元，零次幂无需特殊返回分支。
    let mut result = FIXED_ONE;
    while exponent != 0 {
        // 2. 只把二进制指数中的置位幂乘入结果，总工作量为 O(log exponent)。
        if exponent & 1 != 0 {
            result = fixed_multiply(result, base);
        }
        exponent >>= 1;
        if exponent == 0 {
            break;
        }
        // 3. 每轮平方生成下一项 x^(2^i)，并按 Linux FSHIFT 规则就近舍入。
        base = fixed_multiply(base, base);
    }
    result
}

fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut next = (load as u128) * exp as u128 + (active as u128) * (FIXED_ONE - exp) as u128;
    // Linux 在 active 上升时向上舍入，避免低负载长期因 fixed-point 截断而停滞。
    if active >= load {
        next += (FIXED_ONE - 1) as u128;
    }
    u64::try_from(next / FIXED_ONE as u128).expect("load-average EWMA overflow")
}

/// @description 在到期 tick 上唯一采样全局 runnable Task 并提交 Linux fixed-point EWMA。
///
/// @param now_us 本次 timer deferred batch 固定的 monotonic 微秒时刻。
/// @return 未到期或其他 hart 已 claim 时立即返回；claimant 完成本批提交后返回。
/// @errors cadence、task count 或 EWMA 数值耗尽可表达范围时 fail-stop。
pub(super) fn update(now_us: u64) {
    // 1. 非到期 tick 只执行 atomic load；不关闭中断、不争抢全局 value lock。
    let Some(claim) = TASK_MANAGER.load_average.claim(now_us) else {
        return;
    };
    // 2. 每次在 graph lock 内只取一个 Arc，然后解锁读 SchedulingState。
    //    缺失 cursor 会需要无界 Task Vec snapshot，timer softirq OOM 时无法返回错误。
    let mut cursor = None;
    let mut runnable = 0;
    loop {
        let next = {
            let graph = TASK_MANAGER.graph.lock();
            let same_process = cursor.and_then(|(pid, tid)| {
                let node = graph.nodes.get(&pid)?;
                let ProcessState::Live(threads) = &node.state else {
                    return None;
                };
                threads
                    .iter_after(&tid)
                    .next()
                    .map(|(&tid, task)| ((pid, tid), task.clone()))
            });
            same_process.or_else(|| {
                let mut nodes = match cursor {
                    Some((pid, _)) => graph.nodes.iter_after(&pid),
                    None => graph.nodes.iter(),
                };
                nodes.find_map(|(&pid, node)| {
                    let ProcessState::Live(threads) = &node.state else {
                        return None;
                    };
                    threads
                        .iter()
                        .next()
                        .map(|(&tid, task)| ((pid, tid), task.clone()))
                })
            })
        };
        let Some((position, task)) = next else {
            break;
        };
        cursor = Some(position);
        if matches!(
            task.scheduling.state.lock().run_state,
            RunState::New
                | RunState::Ready { .. }
                | RunState::Running { .. }
                | RunState::Preempting { .. }
                | RunState::WakePending { .. }
                | RunState::StopPending { .. }
        ) {
            runnable += 1;
        }
    }
    // 3. missed periods 由 O(log n) fixed power 一次提交。
    TASK_MANAGER.load_average.commit(claim, runnable);
}
