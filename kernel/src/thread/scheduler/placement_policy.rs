//! 选核的纯策略：按负载选 CPU，并周期性地在负载并列时让任务轮转到另一个 CPU。
//!
//! # 要解决的问题
//!
//! 任务被抢占后重新进入 Ready 时会按各 CPU 的负载重新选核，所以大的负载差会被自动修正。但负载并列时
//! 倾向于留在原 CPU（`last`），以保持缓存亲和、避免无谓迁移。N+1 个 CPU 密集任务跑在 N 个 CPU 上时
//! 这条规则会把分布永远锁死：几个任务独占一个 CPU，另外两个对半分一个 CPU，三者的 CPU 时间是
//! 100% / 50% / 50%，而不是各 N/(N+1)。
//!
//! # 做法：周期性放宽亲和偏置
//!
//! 每个 CPU 每发生 `ROTATION_INTERVAL_PREEMPTIONS` 次抢占，就可以在负载并列时放宽亲和偏置，
//! 选取 `last` 之外的 CPU。放宽只发生在本核还有竞争线程时，单个任务自己 yield 不会无谓地来回迁移。
//!
//! 迁移者必须先在新 CPU 累积一个轮转窗口的实际执行时间，轮转机会才能再次选中它。
//! 否则 2 个线程在每核交替执行时，固定抢占节拍会反复选中同一个刚迁入的线程，
//! 其他线程仍长期独占各自的 CPU。严格更低负载的目标不受这一限制。

/// 同一 CPU 在多少次抢占后允许放宽亲和偏置。
///
/// 竞争时间片为 10 ms，所以约每 100 ms 轮转一次；再短会让缓存亲和形同虚设，
/// 再长则公平性收敛太慢。
pub(crate) const ROTATION_INTERVAL_PREEMPTIONS: u32 = 10;

/// 线程在本核执行满一个轮转窗口后，才可为负载并列再次迁移。
pub(crate) const fn can_rotate(runtime_on_cpu_ns: u64, quantum_ns: u64) -> bool {
    runtime_on_cpu_ns >= quantum_ns.saturating_mul(ROTATION_INTERVAL_PREEMPTIONS as u64)
}

/// 推进一个 CPU 的抢占计数。
///
/// # Parameters
///
/// - `since_rotation`: 自上次放宽以来该 CPU 已发生的抢占次数。
///
/// # Returns
///
/// `(本次是否放宽, 新的计数)`；计数达到间隔时放宽并归零。
pub(crate) const fn advance_rotation(since_rotation: u32) -> (bool, u32) {
    let next = since_rotation.saturating_add(1);
    if next >= ROTATION_INTERVAL_PREEMPTIONS {
        (true, 0)
    } else {
        (false, next)
    }
}

/// 在候选 CPU 中按负载选一个。
///
/// 单趟扫描，不分配。扫描顺序只用于在并列时打破平局，调用方轮换起点以分散新任务。
///
/// # Parameters
///
/// - `candidates`: `(CPU 序号, 瞬时负载)`，只含 active 且 affinity 允许的 CPU，按扫描顺序给出。
/// - `last`: 任务上次所在的 CPU；不在候选中或未知时为 `None`。
/// - `rotate`: 是否放宽亲和偏置。
///
/// # Returns
///
/// 负载严格最小者；多个并列时，`rotate` 为假则优先 `last`，为真则优先第一个不是 `last` 的并列者。
/// 没有候选时为 `None`。
pub(crate) fn choose_cpu(
    candidates: impl Iterator<Item = (usize, usize)>,
    last: Option<usize>,
    rotate: bool,
) -> Option<usize> {
    let mut best_load = usize::MAX;
    let mut first_best = None;
    let mut other_than_last = None;
    let mut last_load = None;
    for (cpu, load) in candidates {
        if Some(cpu) == last {
            last_load = Some(load);
        }
        if load < best_load {
            best_load = load;
            first_best = Some(cpu);
            other_than_last = (Some(cpu) != last).then_some(cpu);
        } else if load == best_load && other_than_last.is_none() && Some(cpu) != last {
            other_than_last = Some(cpu);
        }
    }
    match last.filter(|_| last_load == Some(best_load)) {
        Some(last) if rotate => Some(other_than_last.unwrap_or(last)),
        Some(last) => Some(last),
        None => first_best,
    }
}
