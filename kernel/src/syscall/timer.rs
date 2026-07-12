use crate::{
    syscall::errno::{EFAULT, EINTR, EINVAL},
    task::current_task,
};

/// @description Linux/riscv64 `timespec` 的最小 64 位布局。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TimeSpec {
    pub(crate) tv_sec: i64,
    pub(crate) tv_nsec: i64,
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const ITIMER_REAL: usize = 0;

/// @description 按 Linux RV64 legacy timeval ABI 返回 realtime 与固定 UTC timezone。
///
/// @param timeval 可选的用户态 16-byte `{ i64 sec, i64 usec }` 输出地址。
/// @param timezone 可选的用户态 8-byte `{ i32 minuteswest, i32 dsttime }` 输出地址。
/// @return 成功返回零；任一非空输出地址不可写返回 `-EFAULT`。
pub(crate) fn sys_gettimeofday(timeval: usize, timezone: usize) -> isize {
    let task = current_task().expect("gettimeofday requires a current task");
    // 1. Linux 先写 timeval；timezone fault 不回滚已经完成的 timeval copyout。
    if timeval != 0 {
        let nanoseconds = crate::timer::get_realtime_ns();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&((nanoseconds / 1_000_000_000) as i64).to_ne_bytes());
        bytes[8..].copy_from_slice(&((nanoseconds % 1_000_000_000 / 1_000) as i64).to_ne_bytes());
        if task.copy_to_user(timeval, &bytes).is_err() {
            return -EFAULT;
        }
    }
    // 2. 当前没有 settimeofday ABI，唯一 timezone policy 为 UTC 且不使用 DST。
    // 缺少该显式策略会迫使 syscall 伪造一份可变 timezone state。
    if timezone != 0 && task.copy_to_user(timezone, &[0u8; 8]).is_err() {
        return -EFAULT;
    }
    0
}

fn decode_timeval(bytes: &[u8]) -> Result<u64, isize> {
    let seconds = i64::from_ne_bytes(bytes[..8].try_into().unwrap());
    let microseconds = i64::from_ne_bytes(bytes[8..16].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000).contains(&microseconds) {
        return Err(-EINVAL);
    }
    (seconds as u64)
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(microseconds as u64))
        .ok_or(-EINVAL)
}

fn encode_itimerval(interval_us: u64, value_us: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&(interval_us / 1_000_000).to_ne_bytes());
    bytes[8..16].copy_from_slice(&(interval_us % 1_000_000).to_ne_bytes());
    bytes[16..24].copy_from_slice(&(value_us / 1_000_000).to_ne_bytes());
    bytes[24..].copy_from_slice(&(value_us % 1_000_000).to_ne_bytes());
    bytes
}

/// @description 查询当前 Process 的 Linux ITIMER_REAL。
///
/// @param which 仅接受 `ITIMER_REAL`。
/// @param output 32-byte `itimerval` userspace pointer。
/// @return 成功返回零；selector 或 user-copy 错误返回负 errno。
pub(crate) fn sys_getitimer(which: usize, output: usize) -> isize {
    if which != ITIMER_REAL {
        return -EINVAL;
    }
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    if output == 0 {
        return -EFAULT;
    }
    let (value_us, interval_us) =
        match crate::task::real_timer(task.tgid(), crate::timer::get_time_us()) {
            Ok(value) => value,
            Err(()) => return -EINVAL,
        };
    task.copy_to_user(output, &encode_itimerval(interval_us, value_us))
        .map_or(-EFAULT, |()| 0)
}

/// @description 原子替换当前 Process 的 Linux ITIMER_REAL，并由 timer softirq 发布 SIGALRM。
///
/// @param which 仅接受 `ITIMER_REAL`。
/// @param replacement 32-byte `itimerval` userspace pointer；value 为零时解除定时。
/// @param previous 可选旧值输出 pointer。
/// @return 成功返回零；timeval、selector 或 user-copy 错误返回负 errno。
pub(crate) fn sys_setitimer(which: usize, replacement: usize, previous: usize) -> isize {
    if which != ITIMER_REAL {
        return -EINVAL;
    }
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    if replacement == 0 {
        return -EFAULT;
    }
    let mut bytes = [0u8; 32];
    if task.copy_from_user(replacement, &mut bytes).is_err() {
        return -EFAULT;
    }
    let interval_us = match decode_timeval(&bytes[..16]) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let value_us = match decode_timeval(&bytes[16..]) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let old = match crate::task::set_real_timer(
        task.tgid(),
        value_us,
        interval_us,
        crate::timer::get_time_us(),
    ) {
        Ok(value) => value,
        Err(()) => return -EINVAL,
    };
    if previous != 0
        && task
            .copy_to_user(previous, &encode_itimerval(old.1, old.0))
            .is_err()
    {
        return -EFAULT;
    }
    0
}

/// @description 按相对单调时间挂起当前任务。
///
/// @param req 用户态请求时间；空指针返回 `EFAULT`。
/// @param rem 剩余时间输出地址；当前实现尚不支持中断剩余时间。
/// @return 成功返回零，失败返回负 errno。
pub(crate) fn sys_nanosleep(req: *const TimeSpec, rem: *mut TimeSpec) -> isize {
    if req.is_null() {
        return -EFAULT;
    }

    let Some(task) = current_task() else {
        return -EFAULT;
    };
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    if task.copy_from_user(req as usize, &mut bytes).is_err() {
        return -EFAULT;
    }

    let timespec = decode_timespec(&bytes);
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return -EINVAL;
    }
    let Some(total_ns) = timespec
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(timespec.tv_nsec))
        .and_then(|value| u64::try_from(value).ok())
    else {
        return -EINVAL;
    };

    drop(task);
    let start = crate::timer::get_time_ns();
    let result = if total_ns == 0 {
        0
    } else {
        crate::task::nanosleep(total_ns)
    };
    if result == -EINTR && !rem.is_null() {
        let elapsed = crate::timer::get_time_ns().saturating_sub(start);
        let remaining = total_ns.saturating_sub(elapsed);
        let remaining_spec = TimeSpec {
            tv_sec: (remaining / 1_000_000_000) as i64,
            tv_nsec: (remaining % 1_000_000_000) as i64,
        };
        let Some(task) = current_task() else {
            return -EFAULT;
        };
        if task
            .copy_to_user(rem as usize, &encode_timespec(remaining_spec))
            .is_err()
        {
            return -EFAULT;
        }
    }
    result
}

/// @description Linux/riscv64 clock_gettime，仅支持 realtime 与 monotonic。
///
/// @param clock_id 0 为 CLOCK_REALTIME，1 为 CLOCK_MONOTONIC。
/// @param result 用户态 timespec 输出地址。
/// @return 成功返回 0，非法 clock ID 返回 -EINVAL，copyout fault 返回 -EFAULT。
pub(crate) fn sys_clock_gettime(clock_id: i32, result: *mut TimeSpec) -> isize {
    if result.is_null() {
        return -EFAULT;
    }
    let nanoseconds = match clock_id {
        CLOCK_REALTIME => crate::timer::get_realtime_ns(),
        CLOCK_MONOTONIC => crate::timer::get_time_ns(),
        _ => return -EINVAL,
    };
    let value = TimeSpec {
        tv_sec: (nanoseconds / 1_000_000_000) as i64,
        tv_nsec: (nanoseconds % 1_000_000_000) as i64,
    };
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    if task
        .copy_to_user(result as usize, &encode_timespec(value))
        .is_err()
    {
        -EFAULT
    } else {
        0
    }
}

pub(super) fn decode_timespec(bytes: &[u8; core::mem::size_of::<TimeSpec>()]) -> TimeSpec {
    TimeSpec {
        tv_sec: i64::from_ne_bytes(bytes[..8].try_into().expect("timespec sec width")),
        tv_nsec: i64::from_ne_bytes(bytes[8..].try_into().expect("timespec nsec width")),
    }
}

fn encode_timespec(value: TimeSpec) -> [u8; core::mem::size_of::<TimeSpec>()] {
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    bytes[..8].copy_from_slice(&value.tv_sec.to_ne_bytes());
    bytes[8..].copy_from_slice(&value.tv_nsec.to_ne_bytes());
    bytes
}
