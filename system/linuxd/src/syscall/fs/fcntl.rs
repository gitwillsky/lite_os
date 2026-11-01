use super::*;
use crate::{
    fs::{AdvisoryLockAttempt, AdvisoryLockError, RecordLockMode, RecordLockRange},
    syscall::INTERNAL_RESTART_SYS,
    task::{AdvisoryLockWaitError, wait_for_record_lock},
};

const F_DUPFD: u32 = 0;
const F_GETFD: u32 = 1;
const F_SETFD: u32 = 2;
const F_GETFL: u32 = 3;
const F_SETFL: u32 = 4;
const F_GETLK: u32 = 5;
const F_SETLK: u32 = 6;
const F_SETLKW: u32 = 7;
const F_DUPFD_CLOEXEC: u32 = 1030;
const F_SETPIPE_SZ: u32 = 1031;
const F_GETPIPE_SZ: u32 = 1032;
const F_ADD_SEALS: u32 = 1033;
const F_GET_SEALS: u32 = 1034;
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;
const SEEK_END: i16 = 2;
const FLOCK_SIZE: usize = 32;

#[derive(Clone, Copy)]
struct UserFileLock {
    lock_type: i16,
    whence: i16,
    start: i64,
    length: i64,
    pid: i32,
}

fn read_lock(task: &TaskControlBlock, pointer: usize) -> Result<UserFileLock, isize> {
    if pointer == 0 {
        return Err(-errno::EFAULT);
    }
    let mut bytes = [0u8; FLOCK_SIZE];
    task.copy_from_user(pointer, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    Ok(UserFileLock {
        lock_type: i16::from_ne_bytes(bytes[0..2].try_into().unwrap()),
        whence: i16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
        start: i64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
        length: i64::from_ne_bytes(bytes[16..24].try_into().unwrap()),
        pid: i32::from_ne_bytes(bytes[24..28].try_into().unwrap()),
    })
}

fn write_lock(task: &TaskControlBlock, pointer: usize, lock: UserFileLock) -> Result<(), isize> {
    let mut bytes = [0u8; FLOCK_SIZE];
    bytes[0..2].copy_from_slice(&lock.lock_type.to_ne_bytes());
    bytes[2..4].copy_from_slice(&lock.whence.to_ne_bytes());
    bytes[8..16].copy_from_slice(&lock.start.to_ne_bytes());
    bytes[16..24].copy_from_slice(&lock.length.to_ne_bytes());
    bytes[24..28].copy_from_slice(&lock.pid.to_ne_bytes());
    task.copy_to_user(pointer, &bytes)
        .map_err(|_| -errno::EFAULT)
}

fn normalize_range(
    ofd: &OpenFileDescription,
    lock: UserFileLock,
) -> Result<RecordLockRange, isize> {
    let base = match lock.whence {
        SEEK_SET => 0i128,
        SEEK_CUR => i128::from(ofd.position_snapshot().map_err(super::position_error)?),
        SEEK_END => i128::from(ofd.inode_ref().size()),
        _ => return Err(-errno::EINVAL),
    };
    let anchor = base
        .checked_add(i128::from(lock.start))
        .ok_or(-errno::EOVERFLOW)?;
    let (start, end) = if lock.length > 0 {
        (
            anchor,
            Some(
                anchor
                    .checked_add(i128::from(lock.length))
                    .ok_or(-errno::EOVERFLOW)?,
            ),
        )
    } else if lock.length < 0 {
        (
            anchor
                .checked_add(i128::from(lock.length))
                .ok_or(-errno::EOVERFLOW)?,
            Some(anchor),
        )
    } else {
        (anchor, None)
    };
    if start < 0 {
        return Err(-errno::EINVAL);
    }
    if start > i128::from(i64::MAX) {
        return Err(-errno::EOVERFLOW);
    }
    let end = match end {
        Some(end) if end <= start || end > i128::from(i64::MAX) => {
            return Err(-errno::EOVERFLOW);
        }
        Some(end) => Some(end as u64),
        None => None,
    };
    Ok(RecordLockRange {
        start: start as u64,
        end,
    })
}

fn mode(lock_type: i16) -> Result<Option<RecordLockMode>, isize> {
    match lock_type {
        F_RDLCK => Ok(Some(RecordLockMode::Read)),
        F_WRLCK => Ok(Some(RecordLockMode::Write)),
        F_UNLCK => Ok(None),
        _ => Err(-errno::EINVAL),
    }
}

fn lock_error(error: AdvisoryLockError) -> isize {
    match error {
        AdvisoryLockError::Unsupported => -errno::EBADF,
        AdvisoryLockError::NoLocks => -errno::ENOLCK,
        AdvisoryLockError::FileSystem(error) => ferr(error),
    }
}

fn get_lock(task: &TaskControlBlock, ofd: &Arc<OpenFileDescription>, pointer: usize) -> isize {
    let mut user = match read_lock(task, pointer) {
        Ok(lock) => lock,
        Err(error) => return error,
    };
    let Some(requested) = (match mode(user.lock_type) {
        Ok(mode) => mode,
        Err(error) => return error,
    }) else {
        return -errno::EINVAL;
    };
    let range = match normalize_range(ofd, user) {
        Ok(range) => range,
        Err(error) => return error,
    };
    match vfs().record_lock_conflict(&ofd.lock_holder(), task.file_lock_owner(), requested, range) {
        Ok(Some(conflict)) => {
            user.lock_type = match conflict.mode {
                RecordLockMode::Read => F_RDLCK,
                RecordLockMode::Write => F_WRLCK,
            };
            user.whence = SEEK_SET;
            user.start = conflict.range.start as i64;
            user.length = conflict
                .range
                .end
                .map_or(0, |end| (end - conflict.range.start) as i64);
            user.pid = match i32::try_from(conflict.pid) {
                Ok(pid) => pid,
                Err(_) => return -errno::EOVERFLOW,
            };
        }
        Ok(None) => user.lock_type = F_UNLCK,
        Err(error) => return lock_error(error),
    }
    write_lock(task, pointer, user).map_or_else(|error| error, |()| 0)
}

fn set_lock(
    task: &TaskControlBlock,
    fd: usize,
    ofd: &Arc<OpenFileDescription>,
    pointer: usize,
    blocking: bool,
) -> isize {
    let user = match read_lock(task, pointer) {
        Ok(lock) => lock,
        Err(error) => return error,
    };
    let requested = match mode(user.lock_type) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    if requested == Some(RecordLockMode::Read) && ofd.status_flags() & O_ACCMODE == O_WRONLY
        || requested == Some(RecordLockMode::Write) && ofd.status_flags() & O_ACCMODE == O_RDONLY
    {
        return -errno::EBADF;
    }
    let range = match normalize_range(ofd, user) {
        Ok(range) => range,
        Err(error) => return error,
    };
    if let Some(requested) = requested
        && blocking
    {
        return match wait_for_record_lock(ofd, task.file_lock_owner(), requested, range) {
            Ok(()) => task
                .finish_record_lock(fd, ofd)
                .map_or(-errno::EBADF, |()| 0),
            Err(AdvisoryLockWaitError::Interrupted) => INTERNAL_RESTART_SYS,
            Err(AdvisoryLockWaitError::Unsupported) => -errno::EBADF,
            Err(AdvisoryLockWaitError::NoLocks) => -errno::ENOLCK,
            Err(AdvisoryLockWaitError::FileSystem(error)) => ferr(error),
        };
    }
    match vfs().try_record_lock(&ofd.lock_holder(), task.file_lock_owner(), requested, range) {
        Ok(AdvisoryLockAttempt::Acquired { key, wake_waiters }) => {
            if wake_waiters {
                vfs().notify_advisory_lock(key);
            }
            if requested.is_some() {
                task.finish_record_lock(fd, ofd)
                    .map_or(-errno::EBADF, |()| 0)
            } else {
                0
            }
        }
        Ok(AdvisoryLockAttempt::Blocked { .. }) => -errno::EAGAIN,
        Err(error) => lock_error(error),
    }
}

/// 实现 descriptor flags/status、dup 与 POSIX file-table-associated record locks。
///
/// # Parameters
///
/// - `fd`: source descriptor。
/// - `command`: Linux F_* command。
/// - `argument`: command-specific integer 或 `struct flock *`。
///
/// # Returns
///
/// command result 或负 errno/internal restart sentinel。
pub(crate) fn sys_fcntl(fd: usize, command: u32, argument: usize) -> isize {
    let task = current_task().expect("fcntl requires current task");
    let Some(file) = task.fd_get_raw(fd) else {
        return -errno::EBADF;
    };
    if file.is_path()
        && !matches!(
            command,
            F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL
        )
    {
        return -errno::EBADF;
    }
    match command {
        F_DUPFD if argument < task.file_descriptor_limit() => task
            .fd_allocate(file.clone(), argument, false)
            .map_or_else(super::super::file_descriptor_error, |value| value as isize),
        F_GETFD => task
            .fd_flags(fd)
            .map_or(-errno::EBADF, |value| value as isize),
        F_SETFD => task
            .fd_set_flags(fd, argument as u32)
            .map_or(-errno::EBADF, |()| 0),
        F_GETFL => file.status_flags() as isize,
        F_SETFL => {
            file.set_status_flags(argument as u32);
            0
        }
        F_GETLK | F_SETLK | F_SETLKW => {
            let ofd = &file;
            match command {
                F_GETLK => get_lock(&task, ofd, argument),
                F_SETLK => set_lock(&task, fd, ofd, argument, false),
                F_SETLKW => set_lock(&task, fd, ofd, argument, true),
                _ => unreachable!(),
            }
        }
        F_DUPFD_CLOEXEC if argument < task.file_descriptor_limit() => task
            .fd_allocate(file.clone(), argument, true)
            .map_or_else(super::super::file_descriptor_error, |value| value as isize),
        F_SETPIPE_SZ | F_GETPIPE_SZ => {
            let ofd = &file;
            let pipe = match &ofd.kind {
                OpenFileKind::Pipe(endpoint) => Some(endpoint.pipe()),
                OpenFileKind::Device(file) => file.backing_pipe(),
                _ => None,
            };
            let Some(pipe) = pipe else {
                return -errno::EBADF;
            };
            if command == F_GETPIPE_SZ {
                return pipe.capacity() as isize;
            }
            match pipe.resize(argument) {
                Ok(capacity) => capacity as isize,
                Err(crate::ipc::PipeResizeError::TooLarge) => -errno::EPERM,
                Err(crate::ipc::PipeResizeError::Busy) => -errno::EBUSY,
                Err(crate::ipc::PipeResizeError::OutOfMemory) => -errno::ENOMEM,
            }
        }
        F_ADD_SEALS | F_GET_SEALS => {
            let ofd = &file;
            let file = ofd.inode_ref();
            if !matches!(file.data_backing(), crate::fs::DataBacking::Memory(_)) {
                return -errno::EINVAL;
            }
            // Linux memfd_add_seals 的 writable 检查先于 seal mask 校验。
            if command == F_ADD_SEALS && ofd.status_flags() & O_ACCMODE == O_RDONLY {
                return -errno::EPERM;
            }
            if command == F_GET_SEALS {
                match file.seals() {
                    Ok(seals) => seals as isize,
                    Err(error) => ferr(error),
                }
            } else {
                let Ok(seals) = u32::try_from(argument) else {
                    return -errno::EINVAL;
                };
                file.add_seals(seals).map_or_else(ferr, |_| 0)
            }
        }
        _ => -errno::EINVAL,
    }
}
