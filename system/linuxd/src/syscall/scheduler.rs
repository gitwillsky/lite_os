use core::mem::size_of;

use crate::{
    syscall::errno,
    task::{
        SchedulerAffinityError, SchedulerNiceSelector, SchedulerPolicyError,
        SchedulerPolicyRequest, current_task, scheduler_affinity, scheduler_io_priority,
        scheduler_nice, scheduler_policy, scheduler_rr_interval,
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
const IOPRIO_WHO_PROCESS: i32 = 1;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_IDLE: u16 = 3;
const IOPRIO_DATA_MASK: u16 = (1 << IOPRIO_CLASS_SHIFT) - 1;

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
        SchedulerPolicyError::OutOfMemory => -errno::ENOMEM,
        SchedulerPolicyError::Permission => -errno::EPERM,
        SchedulerPolicyError::Busy => -errno::EBUSY,
        SchedulerPolicyError::Unsupported => -errno::EOPNOTSUPP,
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

/// 设置一个 live Thread 的 Linux I/O priority。
///
/// # Parameters
///
/// - `which`: 当前实现支持 `IOPRIO_WHO_PROCESS`。
/// - `who`: 零选择 caller，正数选择calling PID namespace 的 TID。
/// - `priority`: encoded class/data；支持 NONE/RT/BE/IDLE 与 data 0..7。
///
/// # Returns
///
/// 成功返回 0。
///
/// # Errors
///
/// selector/encoding 非法返回 EINVAL；目标不存在或权限不足返回 ESRCH/EPERM。
pub(crate) fn sys_ioprio_set(which: i32, who: i32, priority: i32) -> isize {
    if which != IOPRIO_WHO_PROCESS || who < 0 || priority < 0 || priority > u16::MAX as i32 {
        return -errno::EINVAL;
    }
    let priority = priority as u16;
    let class = priority >> IOPRIO_CLASS_SHIFT;
    let data = priority & IOPRIO_DATA_MASK;
    if class > IOPRIO_CLASS_IDLE || data > 7 || class == 0 && data != 0 {
        return -errno::EINVAL;
    }
    scheduler_io_priority(who as usize, Some(priority))
        .map(|_| 0)
        .unwrap_or_else(policy_error)
}

/// 查询一个 live Thread 的 Linux I/O priority。
///
/// # Parameters
///
/// - `which`: 当前实现支持 `IOPRIO_WHO_PROCESS`。
/// - `who`: 零选择 caller，正数选择calling PID namespace 的 TID。
///
/// # Returns
///
/// encoded priority；未显式设置时为 class NONE 的零。
///
/// # Errors
///
/// selector 非法或目标不存在返回 EINVAL/ESRCH。
pub(crate) fn sys_ioprio_get(which: i32, who: i32) -> isize {
    if which != IOPRIO_WHO_PROCESS || who < 0 {
        return -errno::EINVAL;
    }
    scheduler_io_priority(who as usize, None)
        .map(|priority| priority as isize)
        .unwrap_or_else(policy_error)
}

/// 按 Linux PROCESS/PGRP/USER selector 替换 live Thread nice。
///
/// # Parameters
///
/// - `which`: `PRIO_PROCESS/PRIO_PGRP/PRIO_USER`。
/// - `who`: 零使用 selector 对应的 calling identity，非零使用 TID/PGID/UID。
/// - `nice`: Linux nice；越界值钳制到 -20..19。
///
/// # Returns
///
/// 至少一个目标成功且无后续错误时返回 0。
///
/// # Errors
///
/// selector 非法返回 `-EINVAL`；空集合、身份或额度错误返回 `-ESRCH/-EPERM/-EACCES`。
pub(crate) fn sys_setpriority(which: i32, who: u32, nice: i32) -> isize {
    let selector = match nice_selector(which, who) {
        Ok(selector) => selector,
        Err(error) => return error,
    };
    scheduler_nice(selector, Some(nice))
        .map(|_| 0)
        .unwrap_or_else(policy_error)
}

/// 返回 Linux selector 命中集合中的最高优先级。
///
/// # Parameters
///
/// - `which`: `PRIO_PROCESS/PRIO_PGRP/PRIO_USER`。
/// - `who`: 零使用 selector 对应的 calling identity，非零使用 TID/PGID/UID。
///
/// # Returns
///
/// raw syscall ABI 的 `20 - nice`，范围 1..40。
///
/// # Errors
///
/// selector 非法或集合为空返回 `-EINVAL/-ESRCH`。
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

/// 保留目标 policy，只替换 legacy `sched_priority`。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID，负数非法。
/// - `parameter`: 用户态 4-byte `struct sched_param` 地址。
///
/// # Returns
///
/// 成功返回 0。
///
/// # Errors
///
/// 参数非法返回 `-EINVAL`；copyin 失败返回 `-EFAULT`；目标/权限错误返回 `-ESRCH/-EPERM`。
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

/// 替换目标 Thread 的 legacy scheduler policy 与 priority。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID，负数非法。
/// - `policy`: Linux scheduler policy 与可选 `SCHED_RESET_ON_FORK` bit。
/// - `parameter`: 用户态 4-byte `struct sched_param` 地址。
///
/// # Returns
///
/// 成功返回 0。
///
/// # Errors
///
/// 参数非法返回 `-EINVAL`；copyin 失败返回 `-EFAULT`；目标/权限错误返回 `-ESRCH/-EPERM`。
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

/// 返回目标 Thread 的 legacy scheduler policy。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID，负数非法。
///
/// # Returns
///
/// `SCHED_OTHER`，并在设置时包含 `SCHED_RESET_ON_FORK`。
///
/// # Errors
///
/// selector 非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`。
pub(crate) fn sys_sched_getscheduler(tid: i32) -> isize {
    if tid < 0 {
        return -errno::EINVAL;
    }
    scheduler_policy(tid as usize, SchedulerPolicyRequest::Query)
        .map(|policy| policy as isize)
        .unwrap_or_else(policy_error)
}

/// 返回目标 Thread 的 legacy real-time priority。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID，负数非法。
/// - `parameter`: 用户态 4-byte `struct sched_param` 输出地址。
///
/// # Returns
///
/// 成功返回 0；当前 `SCHED_OTHER` priority 固定为 0。
///
/// # Errors
///
/// 参数非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；copyout 失败返回 `-EFAULT`。
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

/// 返回 Linux policy 的最大 legacy real-time priority。
///
/// # Parameters
///
/// - `policy`: Linux v7.1 scheduler policy，不接受 flag。
///
/// # Returns
///
/// FIFO/RR 返回 99，其他合法 policy 返回 0。
///
/// # Errors
///
/// policy 未定义返回 `-EINVAL`。
pub(crate) fn sys_sched_get_priority_max(policy: i32) -> isize {
    match policy {
        SCHED_FIFO | SCHED_RR => 99,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE | SCHED_EXT => 0,
        _ => -errno::EINVAL,
    }
}

/// 返回 Linux policy 的最小 legacy real-time priority。
///
/// # Parameters
///
/// - `policy`: Linux v7.1 scheduler policy，不接受 flag。
///
/// # Returns
///
/// FIFO/RR 返回 1，其他合法 policy 返回 0。
///
/// # Errors
///
/// policy 未定义返回 `-EINVAL`。
pub(crate) fn sys_sched_get_priority_min(policy: i32) -> isize {
    match policy {
        SCHED_FIFO | SCHED_RR => 1,
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE | SCHED_EXT => 0,
        _ => -errno::EINVAL,
    }
}

/// 返回目标 Thread 的固定 `SCHED_OTHER` 基础时间片。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID，负数非法。
/// - `interval`: 用户态 16-byte `__kernel_timespec` 输出地址。
///
/// # Returns
///
/// 成功返回 0。
///
/// # Errors
///
/// selector 非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；copyout 失败返回 `-EFAULT`。
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

/// 按 Linux 变长 cpumask ABI 替换目标 Thread affinity。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID。
/// - `length`: userspace mask 字节数；短 mask 高位补零，长 mask 截断到 kernel 宽度。
/// - `input`: 用户态 CPU mask 地址。
///
/// # Returns
///
/// 成功返回 0。
///
/// # Errors
///
/// copyin 失败返回 `-EFAULT`；目标/权限/mask 错误返回 `-ESRCH/-EPERM/-EINVAL`。
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

/// 返回目标 Thread 当前可运行的 active logical CPU mask。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数选择calling PID namespace 的 TID。
/// - `length`: 用户缓冲区字节数，必须足够且按 LP64 `unsigned long` 对齐。
/// - `output`: 用户态 CPU mask 输出地址。
///
/// # Returns
///
/// 成功返回实际复制的 8 bytes。
///
/// # Errors
///
/// 长度非法返回 `-EINVAL`；目标不存在返回 `-ESRCH`；输出不可写返回 `-EFAULT`。
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

/// 主动让出处理器。
///
/// # Returns
///
/// 成功返回零。
pub(crate) fn sys_sched_yield() -> isize {
    // 截止时间已过的 nanosleep 即让出 CPU。
    let _ = native_runtime::system::sleep_until(0);
    0
}

const SCHED_ATTR_SIZE: usize = 56;
const SCHED_ATTR_MIN: usize = 48;

/// Linux sched_setattr：复制版本化 sched_attr，未知尾部必须全零；不在持有任务策略锁时访问用户页。
/// # Parameters
/// tid 使用当前 PID namespace，address 指向 sched_attr，flags 必须为零。
/// # Returns
/// 成功为零；尺寸不兼容写回支持尺寸并返回 E2BIG，用户页错误 EFAULT，领域错误按标准 errno 返回。
pub(crate) fn sys_sched_setattr(tid: i32, address: usize, flags: u32) -> isize {
    if tid < 0 || address == 0 || flags != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("sched_setattr requires current task");
    let mut header = [0u8; 4];
    if task.copy_from_user(address, &mut header).is_err() {
        return -errno::EFAULT;
    }
    let size = match u32::from_ne_bytes(header) {
        0 => SCHED_ATTR_MIN,
        size => size as usize,
    };
    let bad_size = || {
        let _ = task.copy_to_user(address, &(SCHED_ATTR_SIZE as u32).to_ne_bytes());
        -errno::E2BIG
    };
    if !(SCHED_ATTR_MIN..=4096).contains(&size) {
        return bad_size();
    }
    let mut bytes = [0u8; SCHED_ATTR_SIZE];
    if task
        .copy_from_user(address, &mut bytes[..size.min(SCHED_ATTR_SIZE)])
        .is_err()
    {
        return -errno::EFAULT;
    }
    let mut offset = SCHED_ATTR_SIZE;
    let mut tail = [0u8; 128];
    while offset < size {
        let count = (size - offset).min(tail.len());
        let Some(position) = address.checked_add(offset) else {
            return -errno::EFAULT;
        };
        if task.copy_from_user(position, &mut tail[..count]).is_err() {
            return -errno::EFAULT;
        }
        if tail[..count].iter().any(|&byte| byte != 0) {
            return bad_size();
        }
        offset += count;
    }
    let u32_at = |offset| {
        u32::from_ne_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("sched_attr field"),
        )
    };
    let u64_at = |offset| {
        u64::from_ne_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("sched_attr field"),
        )
    };
    let flags = u64_at(8);
    if u32_at(4) > i32::MAX as u32 {
        return -errno::EINVAL;
    }
    if flags & (32 | 64) != 0 && size < SCHED_ATTR_SIZE {
        return -errno::EINVAL;
    }
    crate::task::scheduler_attributes(
        tid as usize,
        Some(crate::task::SchedulingAttributes {
            policy: u32_at(4),
            flags,
            nice: (u32_at(16) as i32).clamp(-20, 19),
            priority: u32_at(20),
            runtime: u64_at(24),
            deadline: u64_at(32),
            period: u64_at(40),
        }),
    )
    .map(|_| 0)
    .unwrap_or_else(policy_error)
}

/// Linux sched_getattr：返回配置，不把 CBS 当前剩余额度当作 runtime 配置值。
/// # Parameters
/// tid 为 namespace TID，address 是输出，size 必须在 48..=PAGE_SIZE；当前仅支持零 flags。
/// # Returns
/// 成功零，尺寸/flags 非法 EINVAL、目标不存在 ESRCH、copyout 错误 EFAULT。
pub(crate) fn sys_sched_getattr(tid: i32, address: usize, size: u32, flags: u32) -> isize {
    let size = size as usize;
    if tid < 0 || address == 0 || !(SCHED_ATTR_MIN..=4096).contains(&size) {
        return -errno::EINVAL;
    }
    let attributes = match crate::task::scheduler_attributes(tid as usize, None) {
        Ok(attributes) => attributes,
        Err(error) => return policy_error(error),
    };
    if flags != 0 {
        return -errno::EINVAL;
    }
    let mut bytes = [0u8; SCHED_ATTR_SIZE];
    bytes[..4].copy_from_slice(&(size.min(SCHED_ATTR_SIZE) as u32).to_ne_bytes());
    bytes[4..8].copy_from_slice(&attributes.policy.to_ne_bytes());
    bytes[8..16].copy_from_slice(&attributes.flags.to_ne_bytes());
    bytes[16..20].copy_from_slice(&attributes.nice.to_ne_bytes());
    bytes[20..24].copy_from_slice(&attributes.priority.to_ne_bytes());
    bytes[24..32].copy_from_slice(&attributes.runtime.to_ne_bytes());
    bytes[32..40].copy_from_slice(&attributes.deadline.to_ne_bytes());
    bytes[40..48].copy_from_slice(&attributes.period.to_ne_bytes());
    // 未启用 util clamp，按标准默认范围投影，不宣称具有调频或任务夹紧机制。
    bytes[52..56].copy_from_slice(&1024u32.to_ne_bytes());
    let task = current_task().expect("sched_getattr requires current task");
    if task
        .copy_to_user(address, &bytes[..size.min(SCHED_ATTR_SIZE)])
        .is_err()
    {
        return -errno::EFAULT;
    }
    let zero = [0u8; 128];
    let mut offset = SCHED_ATTR_SIZE;
    while offset < size {
        let count = (size - offset).min(zero.len());
        let Some(position) = address.checked_add(offset) else {
            return -errno::EFAULT;
        };
        if task.copy_to_user(position, &zero[..count]).is_err() {
            return -errno::EFAULT;
        }
        offset += count;
    }
    0
}
