use crate::{
    syscall::errno,
    task::{
        SignalAction, SignalSendError, SignalWaitError, current_task, send_process_signal,
        send_thread_signal, send_tid_signal, wait_for_signal, wait_for_signal_delivery,
    },
};

use super::timer::{TimeSpec, decode_timespec};

#[repr(C)]
#[derive(Clone, Copy)]
struct UserSigAction {
    handler: usize,
    flags: usize,
    mask: u64,
}

/// @description 实现 Linux process/process-group signal selector 与 signal-zero probe。
///
/// @param pid `>0` 为 TGID，`0` 为 caller PGID，`-1` 为除 init/caller 外全部，`<-1` 为 PGID。
/// @param signal Linux signal number；零只做 existence 与 fixed-root permission probe。
/// @return 至少一个 live Process 匹配返回零；否则返回标准负 errno。
pub(crate) fn sys_kill(pid: i32, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    match send_process_signal(pid, signal) {
        Ok(()) => 0,
        Err(SignalSendError::InvalidSignal) => -errno::EINVAL,
        Err(SignalSendError::NotFound) => -errno::ESRCH,
    }
}

/// @description 实现 Linux RV64 `rt_sigaction` 的 disposition 查询与替换。
///
/// @param signal Linux signal number。
/// @param action 新 24-byte action 地址；零表示仅查询。
/// @param old_action 旧 action 输出地址；零表示不输出。
/// @param signal_set_size userspace sigset 大小，必须为 8。
/// @return 成功返回零，失败返回负 errno。
pub(crate) fn sys_rt_sigaction(
    signal: usize,
    action: usize,
    old_action: usize,
    signal_set_size: usize,
) -> isize {
    if signal_set_size != 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("rt_sigaction requires current task");
    let replacement = if action == 0 {
        None
    } else {
        let mut bytes = [0u8; core::mem::size_of::<UserSigAction>()];
        if task.copy_from_user(action, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        // SAFETY: bytes has the exact ABI size; read_unaligned yields an owned value.
        let action = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<UserSigAction>()) };
        Some(SignalAction {
            handler: action.handler,
            flags: action.flags,
            mask: action.mask,
        })
    };
    let old = match task.signal_action(signal, replacement) {
        Ok(old) => old,
        Err(()) => return -errno::EINVAL,
    };
    if old_action != 0 {
        let old = UserSigAction {
            handler: old.handler,
            flags: old.flags,
            mask: old.mask,
        };
        // SAFETY: UserSigAction is repr(C), fully initialized, and copied only for this call.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&old as *const UserSigAction).cast::<u8>(),
                core::mem::size_of::<UserSigAction>(),
            )
        };
        if task.copy_to_user(old_action, bytes).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

/// @description 实现当前 Thread 的 Linux `rt_sigprocmask`。
///
/// @param how `SIG_BLOCK/UNBLOCK/SETMASK` 对应值。
/// @param set 新 mask 地址；零表示仅查询。
/// @param old_set 旧 mask 输出地址；零表示不输出。
/// @param signal_set_size userspace sigset 大小，必须为 8。
/// @return 成功返回零，失败返回负 errno。
pub(crate) fn sys_rt_sigprocmask(
    how: usize,
    set: usize,
    old_set: usize,
    signal_set_size: usize,
) -> isize {
    if signal_set_size != 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("rt_sigprocmask requires current task");
    let replacement = if set == 0 {
        None
    } else {
        let mut bytes = [0u8; 8];
        if task.copy_from_user(set, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        Some(u64::from_ne_bytes(bytes))
    };
    let old = match task.signal_mask(how, replacement) {
        Ok(old) => old,
        Err(()) => return -errno::EINVAL,
    };
    if old_set != 0 && task.copy_to_user(old_set, &old.to_ne_bytes()).is_err() {
        return -errno::EFAULT;
    }
    0
}

/// @description 实现 Linux thread-group-aware signal 投递与 signal-zero probe。
///
/// @param tgid 目标 Process ID。
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number。
/// @return 成功返回零，失败返回负 errno。
pub(crate) fn sys_tgkill(tgid: usize, tid: usize, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    if signal == 0 {
        return send_thread_signal(tgid, tid, 0).map_or(-errno::ESRCH, |()| 0);
    }
    send_thread_signal(tgid, tid, signal).map_or(-errno::ESRCH, |()| 0)
}

/// @description 实现 Linux `tkill` 的全局 TID selector，并复用 thread-signal routing。
///
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number；零只做 existence probe。
/// @return 成功返回零；signal 非法返回 `EINVAL`，TID 不存在返回 `ESRCH`。
pub(crate) fn sys_tkill(tid: usize, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    send_tid_signal(tid, signal).map_or(-errno::ESRCH, |()| 0)
}

/// @description 原子安装临时 mask 并等待一个将由 trap-return handler 消费的 signal。
///
/// @param mask 8-byte userspace signal set 地址。
/// @param signal_set_size 必须为 8。
/// @return 捕获 signal 后固定返回 `EINTR`；frame 恢复调用前 mask。
pub(crate) fn sys_rt_sigsuspend(mask: usize, signal_set_size: usize) -> isize {
    if signal_set_size != 8 {
        return -errno::EINVAL;
    }
    if mask == 0 {
        return -errno::EFAULT;
    }
    let task = current_task().expect("rt_sigsuspend requires current task");
    let mut bytes = [0u8; 8];
    if task.copy_from_user(mask, &mut bytes).is_err() {
        return -errno::EFAULT;
    }
    let unblockable = (1u64 << (9 - 1)) | (1u64 << (19 - 1));
    let temporary = u64::from_ne_bytes(bytes) & !unblockable;
    task.begin_signal_suspend(temporary);
    let deliverable = task.caught_signal_set(!temporary);
    drop(task);
    wait_for_signal_delivery(deliverable);
    -errno::EINTR
}

/// @description 实现 Linux RV64 `rt_sigtimedwait` 的 standard-signal 消费与可选 timeout。
///
/// @param set 8-byte signal set 地址。
/// @param info 可选 128-byte `siginfo_t` 输出地址。
/// @param timeout 可选相对 monotonic `timespec` 地址；零表示无限等待。
/// @param signal_set_size userspace sigset 大小，必须为 8。
/// @return 成功返回 signal number；timeout、无关 signal 或用户内存错误返回负 errno。
pub(crate) fn sys_rt_sigtimedwait(
    set: usize,
    info: usize,
    timeout: usize,
    signal_set_size: usize,
) -> isize {
    if signal_set_size != 8 {
        return -errno::EINVAL;
    }
    if set == 0 {
        return -errno::EFAULT;
    }
    let task = current_task().expect("rt_sigtimedwait requires current task");
    let mut set_bytes = [0u8; 8];
    if task.copy_from_user(set, &mut set_bytes).is_err() {
        return -errno::EFAULT;
    }
    let unblockable = (1u64 << (9 - 1)) | (1u64 << (19 - 1));
    let mask = u64::from_ne_bytes(set_bytes) & !unblockable;
    let deadline = if timeout == 0 {
        None
    } else {
        let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
        if task.copy_from_user(timeout, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let timeout = decode_timespec(&bytes);
        if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
            return -errno::EINVAL;
        }
        let Some(relative) = timeout
            .tv_sec
            .checked_mul(1_000_000_000)
            .and_then(|seconds| seconds.checked_add(timeout.tv_nsec))
            .and_then(|value| u64::try_from(value).ok())
        else {
            return -errno::EINVAL;
        };
        let Some(deadline) = crate::timer::get_time_ns().checked_add(relative) else {
            return -errno::EINVAL;
        };
        Some(deadline)
    };
    drop(task);

    let (signal, pending) = match wait_for_signal(mask, deadline) {
        Ok(signal) => signal,
        Err(SignalWaitError::Again) => return -errno::EAGAIN,
        Err(SignalWaitError::Interrupted) => return -errno::EINTR,
    };
    if info != 0 {
        let task = current_task().expect("rt_sigtimedwait resumed without current task");
        if task.copy_to_user(info, &pending.encode(signal)).is_err() {
            return -errno::EFAULT;
        }
    }
    signal as isize
}

/// @description 从当前用户 sp 指向的唯一 RV64 rt frame 恢复 signal 前上下文。
///
/// @return 成功时返回恢复后的用户 a0，坏 frame 返回 `-EFAULT`。
pub(crate) fn sys_rt_sigreturn() -> isize {
    match current_task()
        .expect("rt_sigreturn requires current task")
        .restore_signal_frame()
    {
        Ok(result) => result as isize,
        Err(_) => -errno::EFAULT,
    }
}
