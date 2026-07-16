use super::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    TIMER_ABSTIME, TimeSpec, decode_timespec, encode_timespec,
};
use crate::{
    syscall::errno::{EAGAIN, EFAULT, EINVAL, ENOMEM, EOPNOTSUPP},
    task::{PosixTimerClock, PosixTimerNotification, TimerError, TimerSetting, current_task},
};

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;
const SIGEVENT_BYTES: usize = 64;

fn decode_duration(bytes: &[u8]) -> Result<u64, isize> {
    let value = decode_timespec(bytes.try_into().expect("timespec ABI width"));
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(-EINVAL);
    }
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(value.tv_nsec as u64))
        .ok_or(-EINVAL)
}

fn encode_setting(setting: TimerSetting) -> [u8; 32] {
    let interval = TimeSpec {
        tv_sec: (setting.interval_ns / 1_000_000_000) as i64,
        tv_nsec: (setting.interval_ns % 1_000_000_000) as i64,
    };
    let remaining = TimeSpec {
        tv_sec: (setting.remaining_ns / 1_000_000_000) as i64,
        tv_nsec: (setting.remaining_ns % 1_000_000_000) as i64,
    };
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&encode_timespec(interval));
    bytes[16..].copy_from_slice(&encode_timespec(remaining));
    bytes
}

fn timer_error(error: TimerError, create: bool) -> isize {
    match error {
        TimerError::NotFound => -EINVAL,
        TimerError::OutOfMemory if create => -EAGAIN,
        TimerError::OutOfMemory => -ENOMEM,
        TimerError::Exhausted => -EAGAIN,
    }
}

fn validate_clock(clock_id: i32) -> Result<(), isize> {
    match clock_id {
        CLOCK_REALTIME | CLOCK_MONOTONIC => Ok(()),
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => Err(-EOPNOTSUPP),
        _ => Err(-EINVAL),
    }
}

/// 创建一个 Linux process-owned POSIX timer。
pub(crate) fn sys_timer_create(clock_id: i32, event: usize, output: usize) -> isize {
    if let Err(error) = validate_clock(clock_id) {
        return error;
    }
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    let notification = if event == 0 {
        PosixTimerNotification::Default
    } else {
        let mut bytes = [0u8; SIGEVENT_BYTES];
        if task.copy_from_user(event, &mut bytes).is_err() {
            return -EFAULT;
        }
        let value = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
        let signal = i32::from_ne_bytes(bytes[8..12].try_into().unwrap());
        let notify = i32::from_ne_bytes(bytes[12..16].try_into().unwrap());
        if matches!(notify, SIGEV_SIGNAL | SIGEV_THREAD | SIGEV_THREAD_ID)
            && !(1..=64).contains(&signal)
        {
            return -EINVAL;
        }
        match notify {
            SIGEV_NONE => PosixTimerNotification::None,
            SIGEV_SIGNAL | SIGEV_THREAD => PosixTimerNotification::Process {
                signal: signal as usize,
                value,
            },
            SIGEV_THREAD_ID => {
                let tid = i32::from_ne_bytes(bytes[16..20].try_into().unwrap());
                if tid <= 0 {
                    return -EINVAL;
                }
                PosixTimerNotification::Thread {
                    tid: tid as usize,
                    signal: signal as usize,
                    value,
                }
            }
            _ => return -EINVAL,
        }
    };
    let clock = if clock_id == CLOCK_REALTIME {
        PosixTimerClock::Realtime
    } else {
        PosixTimerClock::Monotonic
    };
    let id = match crate::task::create_posix_timer(task.tgid(), clock, notification) {
        Ok(id) => id,
        Err(error) => return timer_error(error, true),
    };
    if task.copy_to_user(output, &id.to_ne_bytes()).is_err() {
        crate::task::delete_posix_timer(task.tgid(), id)
            .expect("new POSIX timer rollback must find its record");
        return -EFAULT;
    }
    0
}

/// 查询一个 Linux POSIX timer 的 interval 与相对剩余时间。
pub(crate) fn sys_timer_gettime(id: i32, output: usize) -> isize {
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    let setting = match crate::task::posix_timer(task.tgid(), id, crate::timer::get_time_ns()) {
        Ok(setting) => setting,
        Err(error) => return timer_error(error, false),
    };
    task.copy_to_user(output, &encode_setting(setting))
        .map_or(-EFAULT, |()| 0)
}

/// 返回一个 Linux POSIX timer 最近一次 signal delivery 的 overrun count。
pub(crate) fn sys_timer_getoverrun(id: i32) -> isize {
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    crate::task::posix_timer_overrun(task.tgid(), id)
        .map(|overrun| overrun as isize)
        .unwrap_or_else(|error| timer_error(error, false))
}

/// 原子替换一个 Linux POSIX timer setting。
pub(crate) fn sys_timer_settime(id: i32, flags: i32, replacement: usize, previous: usize) -> isize {
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    if replacement == 0 {
        return -EINVAL;
    }
    let mut bytes = [0u8; 32];
    if task.copy_from_user(replacement, &mut bytes).is_err() {
        return -EFAULT;
    }
    let interval_ns = match decode_duration(&bytes[..16]) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let value_ns = match decode_duration(&bytes[16..]) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let now_ns = crate::timer::get_time_ns();
    let old = match crate::task::set_posix_timer(
        task.tgid(),
        id,
        value_ns,
        interval_ns,
        flags & TIMER_ABSTIME != 0,
        now_ns,
    ) {
        Ok(setting) => setting,
        Err(error) => return timer_error(error, false),
    };
    if previous != 0 && task.copy_to_user(previous, &encode_setting(old)).is_err() {
        return -EFAULT;
    }
    0
}

/// 删除一个 Linux POSIX timer 及其 active deadline membership。
pub(crate) fn sys_timer_delete(id: i32) -> isize {
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    crate::task::delete_posix_timer(task.tgid(), id)
        .map_or_else(|error| timer_error(error, false), |()| 0)
}
