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
