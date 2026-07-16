use crate::{
    syscall::errno::{EFAULT, EINTR, EINVAL, ENOMEM, EOPNOTSUPP},
    task::{WaitResult, current_task},
};

mod posix;
pub(crate) use posix::*;

/// @description Linux/riscv64 `timespec` 的最小 64 位布局。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TimeSpec {
    pub(crate) tv_sec: i64,
    pub(crate) tv_nsec: i64,
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
const TIMER_ABSTIME: i32 = 1;
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
    let setting = match crate::task::real_timer(task.tgid(), crate::timer::get_time_ns()) {
        Ok(value) => value,
        Err(_) => return -EINVAL,
    };
    task.copy_to_user(
        output,
        &encode_itimerval(setting.interval_ns / 1_000, setting.remaining_ns / 1_000),
    )
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
        value_us.saturating_mul(1_000),
        interval_us.saturating_mul(1_000),
        crate::timer::get_time_ns(),
    ) {
        Ok(value) => value,
        Err(crate::task::TimerError::NotFound | crate::task::TimerError::Exhausted) => {
            return -EINVAL;
        }
        Err(crate::task::TimerError::OutOfMemory) => return -ENOMEM,
    };
    if previous != 0
        && task
            .copy_to_user(
                previous,
                &encode_itimerval(old.interval_ns / 1_000, old.remaining_ns / 1_000),
            )
            .is_err()
    {
        return -EFAULT;
    }
    0
}

fn timespec_ns(value: TimeSpec) -> Result<u64, isize> {
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(-EINVAL);
    }
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(value.tv_nsec as u64))
        .ok_or(-EINVAL)
}

fn finish_sleep(result: WaitResult, deadline_ns: u64, remaining: *mut TimeSpec) -> isize {
    match result {
        WaitResult::TimedOut => 0,
        WaitResult::Interrupted if remaining.is_null() => -EINTR,
        WaitResult::Interrupted => {
            let nanoseconds = deadline_ns.saturating_sub(crate::timer::get_time_ns());
            let value = TimeSpec {
                tv_sec: (nanoseconds / 1_000_000_000) as i64,
                tv_nsec: (nanoseconds % 1_000_000_000) as i64,
            };
            let Some(task) = current_task() else {
                return -EFAULT;
            };
            task.copy_to_user(remaining as usize, &encode_timespec(value))
                .map_or(-EFAULT, |()| -EINTR)
        }
        WaitResult::Woken => panic!("deadline wait completed without timeout or signal"),
        WaitResult::OutOfMemory => -ENOMEM,
    }
}

/// @description 按相对单调时间挂起当前任务。
///
/// @param req 用户态请求时间；空指针返回 `EFAULT`。
/// @param rem 被 signal 中断时的可选剩余时间输出地址。
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

    let total_ns = match timespec_ns(decode_timespec(&bytes)) {
        Ok(value) => value,
        Err(error) => return error,
    };

    drop(task);
    let start = crate::timer::get_time_ns();
    let Some(deadline) = start.checked_add(total_ns) else {
        return -EINVAL;
    };
    finish_sleep(crate::task::sleep_until(deadline), deadline, rem)
}

/// @description 按 Linux clock selector 执行 relative 或 absolute interruptible sleep。
///
/// @param clock_id 当前支持 `CLOCK_REALTIME` 与 `CLOCK_MONOTONIC`。
/// @param flags `TIMER_ABSTIME` 选择 absolute deadline；Linux 对其他位不赋予语义。
/// @param req 用户态 64-bit timespec 请求地址。
/// @param rem relative sleep 被 signal 中断时的可选剩余时间输出；absolute 模式不修改。
/// @return 到期返回 0；非法时间/clock、未支持 CPU clock、user-copy 或 signal 返回负 errno。
pub(crate) fn sys_clock_nanosleep(
    clock_id: i32,
    flags: i32,
    req: *const TimeSpec,
    rem: *mut TimeSpec,
) -> isize {
    // 1. selector capability 在 user-copy 前确定；否则坏指针会掩盖 invalid/unsupported clock。
    if !matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC) {
        return if matches!(clock_id, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID) {
            -EOPNOTSUPP
        } else {
            -EINVAL
        };
    }
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    if req.is_null() || task.copy_from_user(req as usize, &mut bytes).is_err() {
        return -EFAULT;
    }
    let requested_ns = match timespec_ns(decode_timespec(&bytes)) {
        Ok(value) => value,
        Err(error) => return error,
    };
    drop(task);

    // 2. absolute 值是所选 clock 的 timestamp；若当作 duration，realtime 会多睡一个 epoch。
    let absolute = flags & TIMER_ABSTIME != 0;
    let deadline = if absolute {
        if clock_id == CLOCK_MONOTONIC {
            requested_ns
        } else {
            crate::timer::realtime_deadline_to_monotonic_ns(requested_ns)
        }
    } else {
        match crate::timer::get_time_ns().checked_add(requested_ns) {
            Some(value) => value,
            None => return -EINVAL,
        }
    };
    // 3. Linux absolute sleep 被中断时不写 remaining；保留用户 buffer 原值。
    let remaining_output = if absolute { core::ptr::null_mut() } else { rem };
    finish_sleep(
        crate::task::sleep_until(deadline),
        deadline,
        remaining_output,
    )
}

/// @description 查询 Linux/riscv64 realtime、monotonic 或 calling task CPU clock。
///
/// @param clock_id Linux `CLOCK_REALTIME/MONOTONIC/PROCESS_CPUTIME_ID/THREAD_CPUTIME_ID`。
/// @param result 用户态 timespec 输出地址。
/// @return 成功返回 0，非法 clock ID 返回 -EINVAL，copyout fault 返回 -EFAULT。
pub(crate) fn sys_clock_gettime(clock_id: i32, result: *mut TimeSpec) -> isize {
    let value = match clock_id {
        CLOCK_REALTIME | CLOCK_MONOTONIC => {
            let nanoseconds = if clock_id == CLOCK_REALTIME {
                crate::timer::get_realtime_ns()
            } else {
                crate::timer::get_time_ns()
            };
            TimeSpec {
                tv_sec: (nanoseconds / 1_000_000_000) as i64,
                tv_nsec: (nanoseconds % 1_000_000_000) as i64,
            }
        }
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            let Some(task) = current_task() else {
                return -EFAULT;
            };
            let (process_runtime_us, thread_runtime_us) =
                task.cpu_runtime_snapshot(crate::timer::get_time_us());
            let runtime_us = if clock_id == CLOCK_PROCESS_CPUTIME_ID {
                process_runtime_us
            } else {
                thread_runtime_us
            };
            TimeSpec {
                tv_sec: (runtime_us / 1_000_000) as i64,
                tv_nsec: ((runtime_us % 1_000_000) * 1_000) as i64,
            }
        }
        _ => return -EINVAL,
    };
    if result.is_null() {
        return -EFAULT;
    }
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

/// @description 查询 LiteOS 已实现 Linux clocks 的实际可观察分辨率。
///
/// @param clock_id Linux `CLOCK_REALTIME/MONOTONIC/PROCESS_CPUTIME_ID/THREAD_CPUTIME_ID`。
/// @param result 可为空的用户态 timespec 输出地址；为空时只校验 clock ID。
/// @return 成功返回 0，非法 clock ID 返回 -EINVAL，copyout fault 返回 -EFAULT。
pub(crate) fn sys_clock_getres(clock_id: i32, result: *mut TimeSpec) -> isize {
    let nanoseconds = match clock_id {
        CLOCK_REALTIME | CLOCK_MONOTONIC => crate::timer::monotonic_resolution_ns(),
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => 1_000,
        _ => return -EINVAL,
    };
    if result.is_null() {
        return 0;
    }
    let Some(task) = current_task() else {
        return -EFAULT;
    };
    let value = TimeSpec {
        tv_sec: (nanoseconds / 1_000_000_000) as i64,
        tv_nsec: (nanoseconds % 1_000_000_000) as i64,
    };
    task.copy_to_user(result as usize, &encode_timespec(value))
        .map_or(-EFAULT, |()| 0)
}

pub(super) fn decode_timespec(bytes: &[u8; core::mem::size_of::<TimeSpec>()]) -> TimeSpec {
    TimeSpec {
        tv_sec: i64::from_ne_bytes(bytes[..8].try_into().expect("timespec sec width")),
        tv_nsec: i64::from_ne_bytes(bytes[8..].try_into().expect("timespec nsec width")),
    }
}

pub(super) fn encode_timespec(value: TimeSpec) -> [u8; core::mem::size_of::<TimeSpec>()] {
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    bytes[..8].copy_from_slice(&value.tv_sec.to_ne_bytes());
    bytes[8..].copy_from_slice(&value.tv_nsec.to_ne_bytes());
    bytes
}
