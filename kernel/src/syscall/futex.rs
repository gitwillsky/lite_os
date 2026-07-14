use crate::{
    syscall::errno,
    task::{FutexWaitError, current_task, futex_requeue, futex_wait, futex_wake},
    timer::{get_realtime_ns, get_time_ns},
};

use super::{
    INTERNAL_RESTART_SYS,
    timer::{TimeSpec, decode_timespec},
};

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;
const FUTEX_REQUEUE: usize = 3;
const FUTEX_CMP_REQUEUE: usize = 4;
const FUTEX_WAIT_BITSET: usize = 9;
const FUTEX_WAKE_BITSET: usize = 10;
const FUTEX_PRIVATE_FLAG: usize = 128;
const FUTEX_CLOCK_REALTIME: usize = 256;
const FUTEX_CMD_MASK: usize = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

fn read_timeout(
    task: &crate::task::TaskControlBlock,
    address: usize,
) -> Result<Option<u64>, isize> {
    if address == 0 {
        return Ok(None);
    }
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| errno::EFAULT)?;
    let timeout = decode_timespec(&bytes);
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(errno::EINVAL);
    }
    timeout
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(timeout.tv_nsec))
        .and_then(|value| u64::try_from(value).ok())
        .map(Some)
        .ok_or(errno::EINVAL)
}

fn relative_deadline(
    task: &crate::task::TaskControlBlock,
    address: usize,
) -> Result<Option<u64>, isize> {
    read_timeout(task, address)?
        .map(|duration| get_time_ns().checked_add(duration).ok_or(errno::EINVAL))
        .transpose()
}

fn absolute_deadline(
    task: &crate::task::TaskControlBlock,
    address: usize,
    realtime: bool,
) -> Result<Option<u64>, isize> {
    let Some(absolute) = read_timeout(task, address)? else {
        return Ok(None);
    };
    if !realtime {
        return Ok(Some(absolute));
    }
    // 1. wait registry 只持有 monotonic deadline，避免 realtime epoch 混入 scheduler clock。
    // 2. 在 syscall 入口把 realtime absolute deadline 转成剩余时长；RTC offset 在当前
    //    系统生命周期内不可调整，因此转换后不会丢失可观察的 clock-set 语义。
    let monotonic_now = get_time_ns();
    let realtime_now = get_realtime_ns();
    Ok(Some(if absolute <= realtime_now {
        monotonic_now
    } else {
        monotonic_now
            .checked_add(absolute - realtime_now)
            .ok_or(errno::EINVAL)?
    }))
}

fn futex_error(error: FutexWaitError) -> isize {
    match error {
        FutexWaitError::Again => errno::EAGAIN,
        FutexWaitError::Fault => errno::EFAULT,
        FutexWaitError::Invalid => errno::EINVAL,
        FutexWaitError::TimedOut => errno::ETIMEDOUT,
        FutexWaitError::Interrupted => errno::EINTR,
        FutexWaitError::OutOfMemory => errno::ENOMEM,
    }
}

/// @description 实现 Linux/riscv64 非 PI futex wait/wake/bitset/requeue 语义。
///
/// @param address source futex word，必须 4-byte aligned 且当前可读。
/// @param operation Linux FUTEX command，可附加 PRIVATE/CLOCK_REALTIME flag。
/// @param value WAIT expected 或 wake count。
/// @param timeout WAIT timespec pointer，或 REQUEUE 的 requeue count。
/// @param target REQUEUE target futex word。
/// @param value3 WAIT/WAKE bitset，或 CMP_REQUEUE expected value。
/// @return 成功返回零或受影响 waiter 数；失败返回负 Linux errno。
pub(crate) fn sys_futex(
    address: usize,
    operation: usize,
    value: u32,
    timeout: usize,
    target: usize,
    value3: u32,
) -> isize {
    if address == 0
        || address & 3 != 0
        || operation & !(0x7f | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0
    {
        return -errno::EINVAL;
    }
    let command = operation & FUTEX_CMD_MASK;
    let private = operation & FUTEX_PRIVATE_FLAG != 0;
    let realtime = operation & FUTEX_CLOCK_REALTIME != 0;
    if realtime && command != FUTEX_WAIT_BITSET {
        return -errno::ENOSYS;
    }
    let task = current_task().expect("futex syscall requires current task");
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT_BITSET {
                value3
            } else {
                FUTEX_BITSET_MATCH_ANY
            };
            if bitset == 0 {
                return -errno::EINVAL;
            }
            let deadline = if command == FUTEX_WAIT {
                relative_deadline(&task, timeout)
            } else {
                absolute_deadline(&task, timeout, realtime)
            };
            let deadline = match deadline {
                Ok(deadline) => deadline,
                Err(error) => return -error,
            };
            match futex_wait(task, address, value, private, deadline, bitset) {
                Ok(()) => 0,
                Err(FutexWaitError::Interrupted) if timeout == 0 => INTERNAL_RESTART_SYS,
                Err(error) => -futex_error(error),
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if command == FUTEX_WAKE_BITSET {
                value3
            } else {
                FUTEX_BITSET_MATCH_ANY
            };
            if bitset == 0 {
                return -errno::EINVAL;
            }
            futex_wake(&task, address, private, value as usize, bitset)
                .map_or_else(|error| -futex_error(error), |count| count as isize)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            let compare = (command == FUTEX_CMP_REQUEUE).then_some(value3);
            futex_requeue(
                &task,
                address,
                target,
                private,
                value as usize,
                timeout as u32 as usize,
                compare,
            )
            .map_or_else(|error| -futex_error(error), |count| count as isize)
        }
        _ => -errno::ENOSYS,
    }
}
