use super::thread_selector::scheduler_thread;
use crate::task::{CpuAffinity, current_task};

/// Linux scheduler affinity operation 的领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerAffinityError {
    NotFound,
    Permission,
    Empty,
}

/// 查询或替换 live Thread affinity，并同步完成已禁止 CPU 的执行 ownership migration。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数使用 Linux calling PID namespace 的 TID selector。
/// - `replacement`: `None` 只查询；`Some` 提交 userspace logical CPU bitmap。
///
/// # Returns
///
/// 当前 active topology 上实际生效的 logical CPU bitmap。
///
/// # Errors
///
/// 目标不存在返回 `NotFound`；set 权限不足返回 `Permission`；mask 无 active CPU 返回 `Empty`。
pub(crate) fn scheduler_affinity(
    tid: usize,
    replacement: Option<usize>,
) -> Result<usize, SchedulerAffinityError> {
    let caller = current_task().ok_or(SchedulerAffinityError::NotFound)?;
    let target = scheduler_thread(tid, &caller).ok_or(SchedulerAffinityError::NotFound)?;
    let Some(bits) = replacement else {
        return Ok(target.scheduling.state.lock().cpu_affinity.effective_bits());
    };
    if caller.scheduler_privilege_for(&target).is_none() {
        return Err(SchedulerAffinityError::Permission);
    }
    let affinity = CpuAffinity::from_user_bits(bits).ok_or(SchedulerAffinityError::Empty)?;
    target.scheduling.state.lock().cpu_affinity = affinity;
    // 内核在返回前迁移调用线程自身；其他线程在下一个调度点离开已禁止的 CPU。
    crate::task::processor::apply_scheduling(&target);
    Ok(target.scheduling.state.lock().cpu_affinity.effective_bits())
}
