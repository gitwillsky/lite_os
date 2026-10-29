//! 标准 PIDFD raw flags/int codec 与 errno；实际 identity、权限、文件及生命周期归对应 owner。
use crate::{
    execution::task::{PidFdError, current_task},
    syscall::errno,
};

pub(in crate::syscall) fn error(error: PidFdError) -> isize {
    -match error {
        PidFdError::NotFound => errno::ESRCH,
        PidFdError::NotLeader => errno::ENOENT,
        PidFdError::Permission => errno::EPERM,
        PidFdError::Interrupted => errno::EINTR,
        PidFdError::NoMemory => errno::ENOMEM,
        PidFdError::BadDescriptor => errno::EBADF,
        PidFdError::Fault => errno::EFAULT,
        PidFdError::File(error) => return super::file_descriptor_error(error),
    }
}

pub(crate) fn sys_pidfd_open(pid: i32, flags: u32) -> isize {
    const PIDFD_THREAD: u32 = 0x80;
    if pid <= 0 || flags & !(crate::fs::O_NONBLOCK | PIDFD_THREAD) != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pidfd_open requires current Thread");
    let identity = match crate::execution::task::pidfd_identity(pid, &task) {
        Ok(pid) => pid,
        Err(failure) => return error(failure),
    };
    crate::execution::task::open_pidfd(identity, flags & PIDFD_THREAD != 0, flags, &task)
        .map_or_else(error, |fd| fd as isize)
}

pub(crate) fn sys_pidfd_getfd(pidfd: i32, descriptor: i32, flags: u32) -> isize {
    if flags != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pidfd_getfd requires current Thread");
    let Some(file) = task.fd_get(pidfd as usize) else {
        return -errno::EBADF;
    };
    let identity = match &file.kind {
        crate::file::OpenFileKind::Device(backend) => match backend.pid_reference() {
            Some(identity) => identity,
            None => return -errno::EBADF,
        },
        _ => return -errno::EBADF,
    };
    let captured =
        match crate::execution::task::pidfd_get_file(identity, descriptor as usize, &task) {
            Ok(file) => file,
            Err(failure) => return error(failure),
        };
    task.fd_receive(captured, None, true)
        .map_or_else(super::file_descriptor_error, |fd| fd as isize)
}

pub(crate) fn sys_pidfd_send_signal(pidfd: i32, signal: i32, info: usize, flags: u32) -> isize {
    use crate::execution::task::PidSignalScope;
    if flags & !7 != 0 || flags.count_ones() > 1 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pidfd_send_signal requires current Thread");
    let (identity, mut scope) = match pidfd {
        -10000 => (task.tid_identity(), PidSignalScope::Thread),
        -10001 => (task.tgid_identity().clone(), PidSignalScope::ThreadGroup),
        _ => {
            let Some(file) = task.fd_get(pidfd as usize) else {
                return -errno::EBADF;
            };
            let identity = match &file.kind {
                crate::file::OpenFileKind::Device(backend) => match backend.pid_reference() {
                    Some(identity) => identity.clone(),
                    None => return -errno::EBADF,
                },
                crate::file::OpenFileKind::Inode => {
                    match file.inode_ref().process_directory_identity() {
                        Some(identity) => identity.clone(),
                        None => return -errno::EBADF,
                    }
                }
                _ => return -errno::EBADF,
            };
            if identity.number_in(&task.pid_namespace()) == 0 {
                return -errno::EINVAL;
            }
            (
                identity,
                if file.status_flags() & 0x80 != 0 {
                    PidSignalScope::Thread
                } else {
                    PidSignalScope::ThreadGroup
                },
            )
        }
    };
    scope = match flags {
        1 => PidSignalScope::Thread,
        2 => PidSignalScope::ThreadGroup,
        4 => PidSignalScope::ProcessGroup,
        _ => scope,
    };
    let info = if info == 0 {
        None
    } else {
        match super::signal::queued_signal_info(info) {
            Ok((signo, info)) if signo == signal => Some(info),
            Ok(_) => return -errno::EINVAL,
            Err(error) => return error,
        }
    };
    crate::execution::task::send_pid_signal(&identity, scope, signal as usize, info)
        .map_or_else(super::signal::signal_send_errno, |()| 0)
}
