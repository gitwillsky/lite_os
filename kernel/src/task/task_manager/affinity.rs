use super::thread_selector::scheduler_thread;
use crate::task::{
    CpuAffinity, current_task,
    processor::{replace_task_affinity, request_task_reschedule},
    suspend_current_and_run_next,
};

/// @description Linux scheduler affinity operation 的领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerAffinityError {
    NotFound,
    Permission,
    Empty,
}

/// @description 查询或替换 live Thread affinity，并同步完成已禁止 CPU 的执行 ownership migration。
///
/// @param tid 零选择 calling Thread；正数使用 Linux 全局 TID selector。
/// @param replacement `None` 只查询；`Some` 提交 userspace logical CPU bitmap。
/// @return 当前 active topology 上实际生效的 logical CPU bitmap。
/// @errors 目标不存在返回 `NotFound`；set 权限不足返回 `Permission`；mask 无 active CPU 返回 `Empty`。
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
    replace_task_affinity(&target, affinity);

    // 1. Ready membership 已在 replace 中换代；这里只等待执行或切出 owner 离开禁止 CPU。
    // 2. self-target 直接 yield，使 syscall continuation 在允许 CPU 恢复；remote target 先发 IPI。
    // 3. caller 以正常 CFS membership 轮转等待，不 busy-spin，也不引入第二套 completion owner。
    while target.scheduling.state.lock().executes_outside_affinity() {
        if !alloc::sync::Arc::ptr_eq(&caller, &target) {
            request_task_reschedule(&target);
        }
        suspend_current_and_run_next();
    }
    Ok(target.scheduling.state.lock().cpu_affinity.effective_bits())
}
