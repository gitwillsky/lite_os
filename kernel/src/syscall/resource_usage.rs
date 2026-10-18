//! `getrusage`：按 Linux `struct rusage` 布局报告 CPU 时间。

use crate::{syscall::errno, task::current_task};

const RUSAGE_SELF: i32 = 0;
const RUSAGE_CHILDREN: i32 = -1;
const RUSAGE_THREAD: i32 = 1;
/// `struct rusage`：两个 `timeval` 加 14 个 `long`。
const RUSAGE_BYTES: usize = 144;

/// 报告 calling Process、已回收 children 或 calling Thread 的资源使用。
///
/// 内核只记账总 CPU runtime，没有 user/system 拆分，所以全部计入 `ru_utime`，`ru_stime` 恒为 0；
/// 其余计数字段（maxrss、缺页、上下文切换等）恒为 0。这些限制记录在 syscall 支持矩阵里。
///
/// # Parameters
///
/// - `who`: `RUSAGE_SELF`、`RUSAGE_CHILDREN` 或 `RUSAGE_THREAD`。
/// - `usage`: 用户态 `struct rusage` 输出地址。
///
/// # Returns
///
/// 成功返回 0；`who` 非法返回 `-EINVAL`；输出地址不可写返回 `-EFAULT`。
pub(crate) fn sys_getrusage(who: i32, usage: usize) -> isize {
    let task = current_task().expect("getrusage requires a current task");
    let runtime_us = match who {
        RUSAGE_SELF | RUSAGE_THREAD => {
            let (process, thread) = task.cpu_runtime_snapshot(crate::timer::get_time_us());
            if who == RUSAGE_SELF { process } else { thread }
        }
        RUSAGE_CHILDREN => task.reaped_children_runtime_us(),
        _ => return -errno::EINVAL,
    };
    let bytes = encode_rusage(runtime_us);
    if task.copy_to_user(usage, &bytes).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}

fn encode_rusage(utime_us: u64) -> [u8; RUSAGE_BYTES] {
    // 布局按 u64 字段排列：ru_utime.{tv_sec,tv_usec}，其后全为零。
    let mut fields = [0u64; RUSAGE_BYTES / size_of::<u64>()];
    fields[0] = utime_us / 1_000_000;
    fields[1] = utime_us % 1_000_000;
    let mut bytes = [0u8; RUSAGE_BYTES];
    for (chunk, field) in bytes.chunks_exact_mut(size_of::<u64>()).zip(fields) {
        chunk.copy_from_slice(&field.to_ne_bytes());
    }
    bytes
}
