use crate::execution::sync::WaitResult;
use crate::{
    execution::task::{
        PendingSignal, SignalAction, SignalSendError, SignalStack, SignalStackError,
        SignalWaitError, current_task, send_process_signal, send_queued_process_signal,
        send_queued_thread_signal, send_thread_signal, send_tid_signal, wait_for_signal,
        wait_for_signal_delivery,
    },
    syscall::errno,
};

use super::timer::{TimeSpec, decode_timespec, timespec_ns};

#[repr(C)]
#[derive(Clone, Copy)]
struct UserSignalStack {
    sp: usize,
    flags: i32,
    padding: u32,
    size: usize,
}

const _: () = assert!(core::mem::size_of::<UserSignalStack>() == 24);

/// 实现 Linux process/process-group signal selector 与 signal-zero probe。
///
/// # Parameters
///
/// - `pid`: `>0` 为 TGID，`0` 为 caller PGID，`-1` 为除 init/caller 外全部，`<-1` 为 PGID。
/// - `signal`: Linux signal number；零只做 existence 与 fixed-root permission probe。
///
/// # Returns
///
/// 至少一个 live Process 匹配返回零；否则返回标准负 errno。
pub(crate) fn sys_kill(pid: i32, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    match send_process_signal(pid, signal) {
        Ok(()) => 0,
        Err(SignalSendError::InvalidSignal) => -errno::EINVAL,
        Err(SignalSendError::NotFound) => -errno::ESRCH,
        Err(SignalSendError::Permission) => -errno::EPERM,
        Err(SignalSendError::Again) => -errno::EAGAIN,
    }
}

/// 实现 Linux LP64 `rt_sigaction` 的 disposition 查询与替换。
///
/// # Parameters
///
/// - `signal`: Linux signal number。
/// - `action`: architecture-owned action 地址；零表示仅查询。
/// - `old_action`: 同一标准布局的旧 action 输出地址；零表示不输出。
/// - `signal_set_size`: userspace sigset 大小，必须为 8。
///
/// # Returns
///
/// 成功返回零，失败返回负 errno。
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
        let mut bytes = crate::system::SignalActionBytes::zeroed();
        if task.copy_from_user(action, bytes.as_bytes_mut()).is_err() {
            return -errno::EFAULT;
        }
        let (handler, flags, restorer, mask) = bytes.decode();
        Some(SignalAction {
            handler,
            flags,
            restorer,
            mask,
        })
    };
    let old = match task.signal_action(signal, replacement) {
        Ok(old) => old,
        Err(()) => return -errno::EINVAL,
    };
    if old_action != 0 {
        let bytes = crate::system::SignalActionBytes::encode(
            old.handler,
            old.flags,
            old.restorer,
            old.mask,
        );
        if task.copy_to_user(old_action, bytes.as_bytes()).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

/// 实现当前 Thread 的 Linux `rt_sigprocmask`。
///
/// # Parameters
///
/// - `how`: `SIG_BLOCK/UNBLOCK/SETMASK` 对应值。
/// - `set`: 新 mask 地址；零表示仅查询。
/// - `old_set`: 旧 mask 输出地址；零表示不输出。
/// - `signal_set_size`: userspace sigset 大小，必须为 8。
///
/// # Returns
///
/// 成功返回零，失败返回负 errno。
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

/// 实现 Linux thread-group-aware signal 投递与 signal-zero probe。
///
/// # Parameters
///
/// - `tgid`: 目标 Process ID。
/// - `tid`: 目标 Thread ID。
/// - `signal`: Linux signal number。
///
/// # Returns
///
/// 成功返回零，失败返回负 errno。
pub(crate) fn sys_tgkill(tgid: usize, tid: usize, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    if tgid as i32 <= 0 || tid as i32 <= 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("tgkill requires current task");
    let Some(tgid) = task.resolve_pid(tgid as u32 as usize) else {
        return -errno::ESRCH;
    };
    let Some(tid) = task.resolve_pid(tid as u32 as usize) else {
        return -errno::ESRCH;
    };
    send_thread_signal(tgid, tid, signal).map_or_else(signal_send_errno, |()| 0)
}

/// 实现 Linux `tkill` 的全局 TID selector，并复用 thread-signal routing。
///
/// # Parameters
///
/// - `tid`: 目标 Thread ID。
/// - `signal`: Linux signal number；零只做 existence probe。
///
/// # Returns
///
/// 成功返回零；signal 非法返回 `EINVAL`，TID 不存在返回 `ESRCH`。
pub(crate) fn sys_tkill(tid: usize, signal: usize) -> isize {
    if signal > 64 {
        return -errno::EINVAL;
    }
    if tid as i32 <= 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("tkill requires current task");
    let Some(tid) = task.resolve_pid(tid as u32 as usize) else {
        return -errno::ESRCH;
    };
    send_tid_signal(tid, signal).map_or_else(signal_send_errno, |()| 0)
}

/// 查询并可选替换当前 Thread 的 Linux LP64 alternate signal stack。
///
/// # Parameters
///
/// - `new_stack`: 新 24-byte `stack_t` 地址；零表示仅查询。
/// - `old_stack`: 旧 `stack_t` 输出地址；零表示不输出。
///
/// # Returns
///
/// 成功返回零；用户地址、active stack、flags 或长度非法时返回负 errno。
pub(crate) fn sys_sigaltstack(new_stack: usize, old_stack: usize) -> isize {
    let task = current_task().expect("sigaltstack requires current task");
    let replacement = if new_stack == 0 {
        None
    } else {
        let mut bytes = [0u8; core::mem::size_of::<UserSignalStack>()];
        if task.copy_from_user(new_stack, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        // SAFETY: bytes has the exact LP64 stack_t size; read_unaligned yields an owned value.
        let stack = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<UserSignalStack>()) };
        Some(SignalStack {
            sp: stack.sp,
            flags: stack.flags,
            size: stack.size,
        })
    };
    let old = match task.signal_stack(replacement) {
        Ok(old) => old,
        Err(SignalStackError::Active) => return -errno::EPERM,
        Err(SignalStackError::InvalidFlags) => return -errno::EINVAL,
        Err(SignalStackError::TooSmall) => return -errno::ENOMEM,
    };
    if old_stack != 0 {
        let old = UserSignalStack {
            sp: old.sp,
            flags: old.flags,
            padding: 0,
            size: old.size,
        };
        // SAFETY: UserSignalStack is repr(C), fully initialized, and copied only for this call.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&old as *const UserSignalStack).cast::<u8>(),
                core::mem::size_of::<UserSignalStack>(),
            )
        };
        if task.copy_to_user(old_stack, bytes).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

pub(in crate::syscall) fn signal_send_errno(error: SignalSendError) -> isize {
    match error {
        SignalSendError::InvalidSignal => -errno::EINVAL,
        SignalSendError::NotFound => -errno::ESRCH,
        SignalSendError::Permission => -errno::EPERM,
        SignalSendError::Again => -errno::EAGAIN,
    }
}

/// 原子安装临时 mask 并等待一个将由 trap-return handler 消费的 signal。
///
/// # Parameters
///
/// - `mask`: 8-byte userspace signal set 地址。
/// - `signal_set_size`: 必须为 8。
///
/// # Returns
///
/// 捕获 signal 后固定返回 `EINTR`；frame 恢复调用前 mask。
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
    match wait_for_signal_delivery(deliverable) {
        WaitResult::OutOfMemory => {
            current_task()
                .expect("sigsuspend OOM without current task")
                .restore_temporary_signal_mask()
                .expect("sigsuspend temporary mask disappeared");
            -errno::ENOMEM
        }
        WaitResult::Woken | WaitResult::Interrupted => -errno::EINTR,
        _ => unreachable!("sigsuspend has no timeout completion"),
    }
}

/// 实现 Linux LP64 `rt_sigtimedwait` 的唯一 pending 消费与可选 timeout。
///
/// # Parameters
///
/// - `set`: 8-byte signal set 地址。
/// - `info`: 可选 128-byte `siginfo_t` 输出地址。
/// - `timeout`: 可选相对 monotonic `timespec` 地址；零表示无限等待。
/// - `signal_set_size`: userspace sigset 大小，必须为 8。
///
/// # Returns
///
/// 成功返回 signal number；timeout、无关 signal 或用户内存错误返回负 errno。
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
        let relative = match timespec_ns(decode_timespec(&bytes)) {
            Ok(relative) => relative,
            Err(error) => return error,
        };
        Some(
            crate::execution::timer::get_time_ns()
                .saturating_add(relative)
                .min(i64::MAX as u64),
        )
    };
    drop(task);

    let (signal, pending) = match wait_for_signal(mask, deadline) {
        Ok(signal) => signal,
        Err(SignalWaitError::Again) => return -errno::EAGAIN,
        Err(SignalWaitError::Interrupted) => return -errno::EINTR,
        Err(SignalWaitError::OutOfMemory) => return -errno::ENOMEM,
    };
    if info != 0 {
        let task = current_task().expect("rt_sigtimedwait resumed without current task");
        if task.copy_to_user(info, &pending.encode(signal)).is_err() {
            return -errno::EFAULT;
        }
    }
    signal as isize
}

/// 从当前用户 sp 指向的唯一 LP64 rt frame 恢复 signal 前上下文。
///
/// # Returns
///
/// 成功时返回恢复后的用户 return register；坏 frame 按架构标准来源强制投递 SIGSEGV 并返回零。
/// 该错误不能作为普通 EFAULT 返回 restorer，否则会继续执行 syscall 后的用户指令。
pub(crate) fn sys_rt_sigreturn() -> isize {
    let task = current_task().expect("rt_sigreturn requires current task");
    task.clear_syscall_restart();
    match task.restore_signal_frame() {
        Ok(result) => result as isize,
        Err(crate::execution::task::SignalFrameRestoreError::OutOfMemory) => {
            drop(task);
            crate::execution::task::exit_current_group_by_signal(syscall_abi::signal::SIGKILL);
        }
        Err(crate::execution::task::SignalFrameRestoreError::Invalid { address }) => {
            let info = match task.signal_frame_fault(address) {
                Ok(info) => info,
                Err(_) => {
                    drop(task);
                    crate::execution::task::exit_current_group_by_signal(
                        syscall_abi::signal::SIGKILL,
                    );
                }
            };
            task.queue_synchronous_fault(syscall_abi::signal::SIGSEGV, info)
                .expect("bad signal frame must force SIGSEGV");
            0
        }
    }
}

/// Linux __copy_siginfo_from_user：只复制 native prefix；未知 layout 追加检查 expansion 为零。
pub(in crate::syscall) fn queued_signal_info(
    address: usize,
) -> Result<(i32, PendingSignal), isize> {
    let task = current_task().ok_or(-errno::ESRCH)?;
    let mut native = [0u8; 48];
    task.copy_from_user(address, &mut native)
        .map_err(|_| -errno::EFAULT)?;
    let info = PendingSignal::from_user_native(&native);
    let signo = i32::from_ne_bytes(native[..4].try_into().expect("si_signo width"));
    if !info.known_user_layout(signo as usize) {
        let mut expansion = [0u8; 80];
        let address = address.checked_add(48).ok_or(-errno::EFAULT)?;
        task.copy_from_user(address, &mut expansion)
            .map_err(|_| -errno::EFAULT)?;
        if expansion.iter().any(|byte| *byte != 0) {
            return Err(-errno::E2BIG);
        }
    }
    Ok((
        i32::from_ne_bytes(native[..4].try_into().expect("si_signo width")),
        info,
    ))
}
/// 标准 rt_sigqueueinfo 的 user-copy/errno seam；来源授权与 publication 归唯一 signal owner。
pub(crate) fn sys_rt_sigqueueinfo(pid: i32, signal: usize, address: usize) -> isize {
    let info = match queued_signal_info(address) {
        Ok((_, info)) => info,
        Err(error) => return error,
    };
    send_queued_process_signal(pid, signal, info).map_or_else(signal_send_errno, |()| 0)
}
/// 标准 rt_tgsigqueueinfo；不存在兼容队列或 caller 伪装 kernel 的入口。
pub(crate) fn sys_rt_tgsigqueueinfo(tgid: i32, tid: i32, signal: usize, address: usize) -> isize {
    let info = match queued_signal_info(address) {
        Ok((_, info)) => info,
        Err(error) => return error,
    };
    send_queued_thread_signal(tgid, tid, signal, info).map_or_else(signal_send_errno, |()| 0)
}

/// 标准 rt_sigpending；size 允许 0..=8，大小为零时不访问用户 pointer。
pub(crate) fn sys_rt_sigpending(address: usize, size: usize) -> isize {
    if size > 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("rt_sigpending requires current task");
    let bytes = task.pending_signal_set().to_ne_bytes();
    if size == 0 {
        return 0;
    }
    task.copy_to_user(address, &bytes[..size])
        .map_or(-errno::EFAULT, |()| 0)
}
