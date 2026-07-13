use crate::{
    fs::{AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockMode, vfs},
    syscall::errno,
    task::{AdvisoryLockWaitError, current_task, wait_for_advisory_lock},
};

use super::pathname::ferr;
use crate::syscall::INTERNAL_RESTART_SYS;

const LOCK_SH: usize = 1;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const LOCK_UN: usize = 8;

fn lock_error(error: AdvisoryLockError) -> isize {
    match error {
        AdvisoryLockError::Unsupported => -errno::EOPNOTSUPP,
        AdvisoryLockError::NoLocks => -errno::ENOLCK,
        AdvisoryLockError::FileSystem(error) => ferr(error),
    }
}

/// @description 按 Linux flock ABI 管理 OFD-associated whole-file advisory lock。
/// @param fd 任意 pathname-backed open file descriptor。
/// @param operation LOCK_SH/LOCK_EX/LOCK_UN，可附加 LOCK_NB。
/// @return 成功返回零；冲突、signal、fd、operation、容量或 backend 错误返回负 errno。
pub(crate) fn sys_flock(fd: usize, operation: usize) -> isize {
    if operation & !(LOCK_SH | LOCK_EX | LOCK_NB | LOCK_UN) != 0 {
        return -errno::EINVAL;
    }
    let command = operation & !LOCK_NB;
    if !matches!(command, LOCK_SH | LOCK_EX | LOCK_UN) {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if command == LOCK_UN {
        return vfs()
            .unlock_advisory_lock(&ofd)
            .map_or_else(lock_error, |_| 0);
    }
    let mode = if command == LOCK_SH {
        AdvisoryLockMode::Shared
    } else {
        AdvisoryLockMode::Exclusive
    };
    if operation & LOCK_NB != 0 {
        return match vfs().try_advisory_lock(&ofd, mode) {
            Ok(AdvisoryLockAttempt::Acquired { key, wake_waiters }) => {
                if wake_waiters {
                    vfs().notify_advisory_lock(key);
                }
                0
            }
            Ok(AdvisoryLockAttempt::Blocked { key, wake_waiters }) => {
                if wake_waiters {
                    vfs().notify_advisory_lock(key);
                }
                -errno::EAGAIN
            }
            Err(error) => lock_error(error),
        };
    }
    match wait_for_advisory_lock(&ofd, mode) {
        Ok(()) => 0,
        Err(AdvisoryLockWaitError::Interrupted) => INTERNAL_RESTART_SYS,
        Err(AdvisoryLockWaitError::Unsupported) => -errno::EOPNOTSUPP,
        Err(AdvisoryLockWaitError::NoLocks) => -errno::ENOLCK,
        Err(AdvisoryLockWaitError::FileSystem(error)) => ferr(error),
    }
}
