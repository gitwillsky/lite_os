use crate::{
    fs::{
        O_CLOEXEC, O_NONBLOCK, OpenFileDescription, OpenFileKind, TimerError, TimerFd, TimerSetting,
    },
    syscall::errno,
    task::{TimerFileClock, create_timer_fd, current_task},
};

use super::timer::{TimeSpec, decode_timespec, encode_timespec, timespec_ns};

const TFD_TIMER_ABSTIME: u32 = 1;

fn timer_error(error: TimerError) -> isize {
    -match error {
        TimerError::NotFound => errno::EBADF,
        TimerError::OutOfMemory => errno::ENOMEM,
        TimerError::Exhausted => errno::EAGAIN,
    }
}

fn encode_setting(setting: TimerSetting) -> [u8; 32] {
    let interval = TimeSpec {
        tv_sec: (setting.interval_ns / 1_000_000_000) as i64,
        tv_nsec: (setting.interval_ns % 1_000_000_000) as i64,
    };
    let value = TimeSpec {
        tv_sec: (setting.remaining_ns / 1_000_000_000) as i64,
        tv_nsec: (setting.remaining_ns % 1_000_000_000) as i64,
    };
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&encode_timespec(interval));
    bytes[16..].copy_from_slice(&encode_timespec(value));
    bytes
}

fn decode_setting(bytes: &[u8; 32]) -> Result<(u64, u64), isize> {
    let interval = decode_timespec(bytes[..16].try_into().expect("itimerspec interval width"));
    let value = decode_timespec(bytes[16..].try_into().expect("itimerspec value width"));
    Ok((timespec_ns(value)?, timespec_ns(interval)?))
}

fn descriptor(fd: usize) -> Result<alloc::sync::Arc<TimerFd>, isize> {
    let task = current_task().ok_or(-errno::ESRCH)?;
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    match &ofd.kind {
        OpenFileKind::TimerFd(timer) => Ok(timer.clone()),
        _ => Err(-errno::EINVAL),
    }
}

/// @description 创建 Linux timerfd OFD，并注册 immutable clock domain。
///
/// @param clock_id 只接受 CLOCK_REALTIME、CLOCK_MONOTONIC 与 CLOCK_BOOTTIME。
/// @param flags 只接受 TFD_NONBLOCK/TFD_CLOEXEC。
/// @return 新 fd；clock、flags、内存或 fd limit 失败返回负 errno。
pub(crate) fn sys_timerfd_create(clock_id: i32, flags: u32) -> isize {
    let clock = match clock_id {
        0 => TimerFileClock::Realtime,
        1 => TimerFileClock::Monotonic,
        7 => TimerFileClock::Boottime,
        _ => return -errno::EINVAL,
    };
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return -errno::EINVAL;
    }
    let backend = match create_timer_fd(clock) {
        Ok(backend) => backend,
        Err(error) => return timer_error(error),
    };
    let timer = match TimerFd::new(backend) {
        Ok(timer) => timer,
        Err(()) => return -errno::ENOMEM,
    };
    let ofd = match OpenFileDescription::timer_fd(timer, flags & O_NONBLOCK) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    let task = current_task().expect("timerfd_create requires current task");
    task.fd_allocate(ofd, flags & O_CLOEXEC != 0)
        .map_or_else(super::file_descriptor_error, |fd| fd as isize)
}

/// @description 原子替换 timerfd setting，并可返回旧的相对 setting。
///
/// @param fd timerfd descriptor。
/// @param flags 只接受 TFD_TIMER_ABSTIME。
/// @param replacement 用户态 itimerspec。
/// @param previous 可为空的旧 setting 输出地址。
/// @return 成功返回零；fd、flag、timespec、copy 或资源错误返回负 errno。
pub(crate) fn sys_timerfd_settime(
    fd: usize,
    flags: u32,
    replacement: usize,
    previous: usize,
) -> isize {
    if flags & !TFD_TIMER_ABSTIME != 0 {
        return -errno::EINVAL;
    }
    let timer = match descriptor(fd) {
        Ok(timer) => timer,
        Err(error) => return error,
    };
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let mut bytes = [0u8; 32];
    if replacement == 0 || task.copy_from_user(replacement, &mut bytes).is_err() {
        return -errno::EFAULT;
    }
    let (value_ns, interval_ns) = match decode_setting(&bytes) {
        Ok(setting) => setting,
        Err(error) => return error,
    };
    let previous_setting = match timer.replace(
        value_ns,
        interval_ns,
        flags & TFD_TIMER_ABSTIME != 0,
        crate::timer::get_time_ns(),
    ) {
        Ok(setting) => setting,
        Err(error) => return timer_error(error),
    };
    if previous != 0
        && task
            .copy_to_user(previous, &encode_setting(previous_setting))
            .is_err()
    {
        return -errno::EFAULT;
    }
    0
}

/// @description 查询 timerfd 当前相对 setting。
///
/// @param fd timerfd descriptor。
/// @param output 用户态 itimerspec 输出地址。
/// @return 成功返回零；fd、copy 或 lifecycle 错误返回负 errno。
pub(crate) fn sys_timerfd_gettime(fd: usize, output: usize) -> isize {
    let timer = match descriptor(fd) {
        Ok(timer) => timer,
        Err(error) => return error,
    };
    if output == 0 {
        return -errno::EFAULT;
    }
    let setting = match timer.setting(crate::timer::get_time_ns()) {
        Ok(setting) => setting,
        Err(error) => return timer_error(error),
    };
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    task.copy_to_user(output, &encode_setting(setting))
        .map_or(-errno::EFAULT, |()| 0)
}
