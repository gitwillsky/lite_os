use crate::{
    syscall::errno,
    task::{FutexWaitError, current_task, futex_wait, futex_wake},
    timer::get_time_ns,
};

use super::timer::{TimeSpec, decode_timespec};

fn timeout_deadline(address: usize) -> Result<Option<u64>, isize> {
    if address == 0 {
        return Ok(None);
    }
    let task = current_task().expect("futex timeout requires current task");
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| errno::EFAULT)?;
    let timeout = decode_timespec(&bytes);
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(errno::EINVAL);
    }
    let duration = timeout
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(timeout.tv_nsec))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(errno::EINVAL)?;
    get_time_ns()
        .checked_add(duration)
        .map(Some)
        .ok_or(errno::EINVAL)
}

/// @description 实现 address-space-keyed Linux futex WAIT/WAKE 子集。
///
/// @param address 4-byte aligned 用户 futex word。
/// @param operation `FUTEX_WAIT/FUTEX_WAKE`，可附加 `FUTEX_PRIVATE_FLAG`。
/// @param value WAIT expected 或 WAKE count。
/// @param timeout WAIT 的可选相对 monotonic timespec。
/// @return WAIT 被唤醒返回零；WAKE 返回数量；失败返回负 errno。
pub(crate) fn sys_futex(address: usize, operation: usize, value: u32, timeout: usize) -> isize {
    const FUTEX_WAIT: usize = 0;
    const FUTEX_WAKE: usize = 1;
    const FUTEX_PRIVATE_FLAG: usize = 128;
    if address == 0 || address & 3 != 0 || operation & !(0x7f | FUTEX_PRIVATE_FLAG) != 0 {
        return -errno::EINVAL;
    }
    match operation & 0x7f {
        FUTEX_WAIT => {
            let deadline = match timeout_deadline(timeout) {
                Ok(deadline) => deadline,
                Err(error) => return -error,
            };
            match futex_wait(address, value, deadline) {
                Ok(()) => 0,
                Err(FutexWaitError::Again) => -errno::EAGAIN,
                Err(FutexWaitError::Fault) => -errno::EFAULT,
                Err(FutexWaitError::Invalid) => -errno::EINVAL,
                Err(FutexWaitError::TimedOut) => -errno::ETIMEDOUT,
                Err(FutexWaitError::Interrupted) => -errno::EINTR,
            }
        }
        FUTEX_WAKE => futex_wake(
            current_task()
                .expect("futex wake requires current task")
                .tgid(),
            address,
            value as usize,
        ) as isize,
        _ => -errno::ENOSYS,
    }
}
