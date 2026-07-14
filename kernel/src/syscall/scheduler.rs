use core::mem::size_of;

use crate::{
    syscall::errno,
    task::{
        SchedulerAffinityError, SchedulerNiceSelector, SchedulerPolicyError,
        SchedulerPolicyRequest, current_task, scheduler_affinity, scheduler_nice, scheduler_policy,
        scheduler_rr_interval, suspend_current_and_run_next,
    },
};

const CPU_MASK_BYTES: usize = size_of::<usize>();
const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;
const SCHED_EXT: i32 = 7;
const PRIO_PROCESS: i32 = 0;
const PRIO_PGRP: i32 = 1;
const PRIO_USER: i32 = 2;

fn affinity_error(error: SchedulerAffinityError) -> isize {
    match error {
        SchedulerAffinityError::NotFound => -errno::ESRCH,
        SchedulerAffinityError::Permission => -errno::EPERM,
        SchedulerAffinityError::Empty => -errno::EINVAL,
    }
}

fn policy_error(error: SchedulerPolicyError) -> isize {
    match error {
        SchedulerPolicyError::Access => -errno::EACCES,
        SchedulerPolicyError::Invalid => -errno::EINVAL,
        SchedulerPolicyError::NotFound => -errno::ESRCH,
        SchedulerPolicyError::Permission => -errno::EPERM,
    }
}

fn nice_selector(which: i32, who: u32) -> Result<SchedulerNiceSelector, isize> {
    match which {
        PRIO_PROCESS => Ok(SchedulerNiceSelector::Process(who)),
        PRIO_PGRP => Ok(SchedulerNiceSelector::Group(who)),
        PRIO_USER => Ok(SchedulerNiceSelector::User(who)),
        _ => Err(-errno::EINVAL),
    }
}

/// @description 按 Linux PROCESS/PGRP/USER selector 替换 live Thread nice。
///
/// @param which `PRIO_PROCESS/PRIO_PGRP/PRIO_USER`。
/// @param who 零使用 selector 对应的 calling identity，非零使用 TID/PGID/UID。
/// @param nice Linux nice；越界值钳制到 -20..19。
/// @return 至少一个目标成功且无后续错误时返回 0。
/// @errors selector 非法返回 `-EINVAL`；空集合、身份或额度错误返回 `-ESRCH/-EPERM/-EACCES`。
pub(crate) fn sys_setpriority(which: i32, who: u32, nice: i32) -> isize {
    let selector = match nice_selector(which, who) {
        Ok(selector) => selector,
        Err(error) => return error,
    };
    scheduler_nice(selector, Some(nice))
        .map(|_| 0)
        .unwrap_or_else(policy_error)
}

/// @description 返回 Linux selector 命中集合中的最高优先级。
///
/// @param which `PRIO_PROCESS/PRIO_PGRP/PRIO_USER`。
/// @param who 零使用 selector 对应的 calling identity，非零使用 TID/PGID/UID。
/// @return raw syscall ABI 的 `20 - nice`，范围 1..40。
/// @errors selector 非法或集合为空返回 `-EINVAL/-ESRCH`。
pub(crate) fn sys_getpriority(which: i32, who: u32) -> isize {
    let selector = match nice_selector(which, who) {
        Ok(selector) => selector,
        Err(error) => return error,
    };
    scheduler_nice(selector, None)
        .map(|nice| (20 - nice) as isize)
        .unwrap_or_else(policy_error)
}

fn read_sched_priority(tid: i32, address: usize) -> Result<i32, isize> {
    if tid < 0 || address == 0 {
        return Err(-errno::EINVAL);
    }
    let task = current_task().expect("scheduler policy syscall requires a current task");
    let mut bytes = [0u8; size_of::<i32>()];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    Ok(i32::from_ne_bytes(bytes))
}

/// @description 保留目标 policy，只替换 legacy `sched_priority`。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID，负数非法。
/// @param parameter 用户态 4-byte `struct sched_param` 地址。
/// @return 成功返回 0。
/// @errors 参数非法返回 `-EINVAL`；copyin 失败返回 `-EFAULT`；目标/权限错误返回 `-ESRCH/-EPERM`。
pub(crate) fn sys_sched_setparam(tid: i32, parameter: usize) -> isize {
    let priority = match read_sched_priority(tid, parameter) {
        Ok(priority) => priority,
        Err(error) => return error,
    };
    scheduler_policy(
        tid as usize,
        SchedulerPolicyRequest::SetParameters { priority },
    )
    .map(|_| 0)
    .unwrap_or_else(policy_error)
}

/// @description 替换目标 Thread 的 legacy scheduler policy 与 priority。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID，负数非法。
/// @param policy Linux scheduler policy 与可选 `SCHED_RESET_ON_FORK` bit。
/// @param parameter 用户态 4-byte `struct sched_param` 地址。
/// @return 成功返回 0。
/// @errors 参数非法返回 `-EINVAL`；copyin 失败返回 `-EFAULT`；目标/权限错误返回 `-ESRCH/-EPERM`。
pub(crate) fn sys_sched_setscheduler(tid: i32, policy: i32, parameter: usize) -> isize {
    if policy < 0 {
        return -errno::EINVAL;
    }
    let priority = match read_sched_priority(tid, parameter) {
        Ok(priority) => priority,
        Err(error) => return error,
    };
    scheduler_policy(
        tid as usize,
        SchedulerPolicyRequest::Replace { policy, priority },
    )
    .map(|_| 0)
    .unwrap_or_else(policy_error)
}

/// @description 返回目标 Thread 的 legacy scheduler policy。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID，负数非法。
/// @return `SCHED_OTHER`，并在设置时包含 `SCHED_RESET_ON_FORK`。
/// @errors selector 非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`。
pub(crate) fn sys_sched_getscheduler(tid: i32) -> isize {
    if tid < 0 {
        return -errno::EINVAL;
    }
    scheduler_policy(tid as usize, SchedulerPolicyRequest::Query)
        .map(|policy| policy as isize)
        .unwrap_or_else(policy_error)
}

/// @description 返回目标 Thread 的 legacy real-time priority。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID，负数非法。
/// @param parameter 用户态 4-byte `struct sched_param` 输出地址。
/// @return 成功返回 0；当前 `SCHED_OTHER` priority 固定为 0。
/// @errors 参数非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；copyout 失败返回 `-EFAULT`。
pub(crate) fn sys_sched_getparam(tid: i32, parameter: usize) -> isize {
    if tid < 0 || parameter == 0 {
        return -errno::EINVAL;
    }
    if let Err(error) = scheduler_policy(tid as usize, SchedulerPolicyRequest::Query) {
        return policy_error(error);
    }
    let task = current_task().expect("sched_getparam requires a current task");
    task.copy_to_user(parameter, &0i32.to_ne_bytes())
        .map_or(-errno::EFAULT, |()| 0)
}

/// @description 返回 Linux policy 的最大 legacy real-time priority。
///
/// @param policy Linux v7.1 scheduler policy，不接受 flag。
/// @return FIFO/RR 返回 99，其他合法 policy 返回 0。
/// @errors policy 未定义返回 `-EINVAL`。
pub(crate) fn sys_sched_get_priority_max(policy: i32) -> isize {
    match policy {
        SCHED_FIFO | SCHED_RR => 99,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE | SCHED_EXT => 0,
        _ => -errno::EINVAL,
    }
}

/// @description 返回 Linux policy 的最小 legacy real-time priority。
///
/// @param policy Linux v7.1 scheduler policy，不接受 flag。
/// @return FIFO/RR 返回 1，其他合法 policy 返回 0。
/// @errors policy 未定义返回 `-EINVAL`。
pub(crate) fn sys_sched_get_priority_min(policy: i32) -> isize {
    match policy {
        SCHED_FIFO | SCHED_RR => 1,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE | SCHED_EXT => 0,
        _ => -errno::EINVAL,
    }
}

/// @description 返回目标 Thread 的固定 `SCHED_OTHER` 基础时间片。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID，负数非法。
/// @param interval 用户态 16-byte `__kernel_timespec` 输出地址。
/// @return 成功返回 0。
/// @errors selector 非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；copyout 失败返回 `-EFAULT`。
pub(crate) fn sys_sched_rr_get_interval(tid: i32, interval: usize) -> isize {
    if tid < 0 {
        return -errno::EINVAL;
    }
    let nanoseconds = match scheduler_rr_interval(tid as usize) {
        Ok(interval) => interval,
        Err(error) => return policy_error(error),
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&((nanoseconds / 1_000_000_000) as i64).to_ne_bytes());
    bytes[8..].copy_from_slice(&((nanoseconds % 1_000_000_000) as i64).to_ne_bytes());
    let task = current_task().expect("sched_rr_get_interval requires a current task");
    task.copy_to_user(interval, &bytes)
        .map_or(-errno::EFAULT, |()| 0)
}

/// @description 按 Linux 变长 cpumask ABI 替换目标 Thread affinity。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID。
/// @param length userspace mask 字节数；短 mask 高位补零，长 mask 截断到 kernel 宽度。
/// @param input 用户态 CPU mask 地址。
/// @return 成功返回 0。
/// @errors copyin 失败返回 `-EFAULT`；目标/权限/mask 错误返回 `-ESRCH/-EPERM/-EINVAL`。
pub(crate) fn sys_sched_setaffinity(tid: i32, length: u32, input: usize) -> isize {
    let mut bytes = [0u8; CPU_MASK_BYTES];
    let count = (length as usize).min(CPU_MASK_BYTES);
    let task = current_task().expect("sched_setaffinity requires a current task");
    if count != 0 && task.copy_from_user(input, &mut bytes[..count]).is_err() {
        return -errno::EFAULT;
    }
    if tid < 0 {
        return -errno::ESRCH;
    }
    scheduler_affinity(tid as usize, Some(usize::from_ne_bytes(bytes)))
        .map(|_| 0)
        .unwrap_or_else(affinity_error)
}

/// @description 返回目标 Thread 当前可运行的 active logical CPU mask。
///
/// @param tid 零选择 calling Thread；正数选择全局 TID。
/// @param length 用户缓冲区字节数，必须足够且按 RV64 `unsigned long` 对齐。
/// @param output 用户态 CPU mask 输出地址。
/// @return 成功返回实际复制的 8 bytes。
/// @errors 长度非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；输出不可写返回 `-EFAULT`。
pub(crate) fn sys_sched_getaffinity(tid: i32, length: u32, output: usize) -> isize {
    let length = length as usize;
    if length < CPU_MASK_BYTES || !length.is_multiple_of(CPU_MASK_BYTES) {
        return -errno::EINVAL;
    }
    if tid < 0 {
        return -errno::ESRCH;
    }
    let mask = match scheduler_affinity(tid as usize, None) {
        Ok(mask) => mask.to_ne_bytes(),
        Err(error) => return affinity_error(error),
    };
    let task = current_task().expect("sched_getaffinity requires a current task");
    if task.copy_to_user(output, &mask).is_err() {
        -errno::EFAULT
    } else {
        CPU_MASK_BYTES as isize
    }
}

/// @description 主动让出处理器。
///
/// @return 成功返回零。
pub(crate) fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}
