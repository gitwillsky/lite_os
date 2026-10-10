use crate::{
    syscall::errno,
    task::{FutexWaitError, current_task, futex_requeue, futex_wait, futex_wake, futex_wake_op},
    timer::{get_realtime_ns, get_time_ns},
};

use super::{
    INTERNAL_RESTART_BLOCK, INTERNAL_RESTART_SYS,
    timer::{TimeSpec, decode_timespec},
};

const FUTEX_WAIT: usize = 0;
const FUTEX_WAKE: usize = 1;
const FUTEX_REQUEUE: usize = 3;
const FUTEX_CMP_REQUEUE: usize = 4;
const FUTEX_WAKE_OP: usize = 5;
const FUTEX_WAIT_BITSET: usize = 9;
const FUTEX_WAKE_BITSET: usize = 10;
const FUTEX_PRIVATE_FLAG: usize = 128;
const FUTEX_CLOCK_REALTIME: usize = 256;
const FUTEX_CMD_MASK: usize = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
mod wake_op;

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
    Ok(Some(
        (timeout.tv_sec as u128 * 1_000_000_000 + timeout.tv_nsec as u128).min(i64::MAX as u128)
            as u64,
    ))
}

fn relative_deadline(
    task: &crate::task::TaskControlBlock,
    address: usize,
) -> Result<Option<u64>, isize> {
    Ok(read_timeout(task, address)?
        .map(|duration| get_time_ns().saturating_add(duration).min(i64::MAX as u64)))
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
        return Ok(Some(
            task.time_offsets()
                .to_host(crate::namespace::TimeClock::Monotonic, absolute),
        ));
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
            .saturating_add(absolute - realtime_now)
            .min(i64::MAX as u64)
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
        FutexWaitError::Unsupported => errno::ENOSYS,
    }
}

/// 实现 Linux 64-bit 非 PI futex wait/wake/bitset/requeue/WAKE_OP 语义。
///
/// # Parameters
///
/// - `address`: 4-byte aligned source；private wake/requeue 不要求实际映射。
/// - `operation`: Linux FUTEX command，可附加 PRIVATE/CLOCK_REALTIME flag。
/// - `value`: WAIT expected 或 wake count。
/// - `timeout`: WAIT timespec pointer、REQUEUE 迁移数或 WAKE_OP 第二队列唤醒数。
/// - `target`: REQUEUE/WAKE_OP target futex word。
/// - `value3`: WAIT/WAKE bitset、CMP_REQUEUE expected 或标准 FUTEX_OP 编码。
///
/// # Returns
///
/// 成功返回零或受影响 waiter 数；失败返回负 Linux errno。
pub(crate) fn sys_futex(
    address: usize,
    operation: usize,
    value: u32,
    timeout: usize,
    target: usize,
    value3: u32,
) -> isize {
    let operation = operation as u32 as usize;
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
            let deadline = if command == FUTEX_WAIT {
                relative_deadline(&task, timeout)
            } else {
                absolute_deadline(&task, timeout, realtime)
            };
            let deadline = match deadline {
                Ok(deadline) => deadline,
                Err(error) => return -error,
            };
            if bitset == 0 {
                return -errno::EINVAL;
            }
            wait_with_deadline(task, address, value, private, deadline, bitset)
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
            // legacy FUTEX_WAKE 即使 nr_wake 为零/负数仍消费首个匹配 waiter。
            futex_wake(
                &task,
                address,
                private,
                (value as i32).max(1) as usize,
                bitset,
            )
            .map_or_else(|error| -futex_error(error), |count| count as isize)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if (value as i32) < 0 || (timeout as u32 as i32) < 0 {
                return -errno::EINVAL;
            }
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
        FUTEX_WAKE_OP => {
            let operation = wake_op::WakeOperation::decode(value3);
            futex_wake_op(
                &task,
                address,
                target,
                private,
                [
                    (value as i32).max(1) as usize,
                    (timeout as u32 as i32).max(1) as usize,
                ],
                operation.update,
                |old| operation.compare(old),
            )
            .map_or_else(|error| -futex_error(error), |count| count as isize)
        }
        _ => -errno::ENOSYS,
    }
}

/// 执行已导入的 WAIT；原始入口与标准 restart_syscall 共用同一期限和 actual wait owner。
pub(super) fn wait_with_deadline(
    task: alloc::sync::Arc<crate::task::TaskControlBlock>,
    address: usize,
    expected: u32,
    private: bool,
    deadline: Option<u64>,
    bitset: u32,
) -> isize {
    match futex_wait(task.clone(), address, expected, private, deadline, bitset) {
        Ok(()) => 0,
        Err(FutexWaitError::Interrupted) => {
            let Some(deadline) = deadline else {
                return INTERNAL_RESTART_SYS;
            };
            task.set_syscall_restart_block(crate::task::SyscallRestartBlock::Futex {
                address,
                expected,
                private,
                deadline,
                bitset,
            });
            INTERNAL_RESTART_BLOCK
        }
        Err(error) => -futex_error(error),
    }
}
