use crate::{
    syscall::errno,
    task::{LinuxSigAction, current_task, send_thread_signal},
};

#[repr(C)]
#[derive(Clone, Copy)]
struct UserSigAction {
    handler: usize,
    flags: usize,
    mask: u64,
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
        Some(LinuxSigAction {
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
