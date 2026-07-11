use crate::{
    syscall::errno,
    task::{FutexWaitError, current_task, futex_wait, futex_wake},
};

/// @description 实现 address-space-keyed Linux futex WAIT/WAKE 子集。
///
/// @param address 4-byte aligned 用户 futex word。
/// @param operation `FUTEX_WAIT/FUTEX_WAKE`，可附加 `FUTEX_PRIVATE_FLAG`。
/// @param value WAIT expected 或 WAKE count。
/// @param timeout 当前 WAIT 必须为空；不伪造 timeout 语义。
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
            if timeout != 0 {
                return -errno::EINVAL;
            }
            match futex_wait(address, value) {
                Ok(()) => 0,
                Err(FutexWaitError::Again) => -errno::EAGAIN,
                Err(FutexWaitError::Fault) => -errno::EFAULT,
                Err(FutexWaitError::Invalid) => -errno::EINVAL,
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
