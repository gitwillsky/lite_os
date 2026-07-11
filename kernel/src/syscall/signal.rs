use crate::{
    signal::{Signal, SignalError, send_signal, sig_return},
    syscall::errno::{EINVAL, EPERM, ESRCH},
    task::current_task,
};

/// @description 向指定任务发送信号。
///
/// @param pid 目标任务 ID。
/// @param sig Linux 信号编号。
/// @return 成功返回零，失败返回负 errno。
pub fn sys_kill(pid: usize, sig: u32) -> isize {
    let Some(signal) = Signal::from_u8(sig as u8) else {
        return -EINVAL;
    };

    match send_signal(pid, signal) {
        Ok(()) => 0,
        Err(SignalError::ProcessNotFound) => -ESRCH,
        Err(SignalError::PermissionDenied) => -EPERM,
        Err(_) => -EINVAL,
    }
}

/// @description 从内核构造的实时信号帧恢复用户上下文。
///
/// @return 成功返回零，失败返回负 errno。
pub fn sys_rt_sigreturn() -> isize {
    let Some(task) = current_task() else {
        return -ESRCH;
    };
    let mut trap_context = task.mm.load_trap_context();
    match sig_return(&task, &mut trap_context) {
        Ok(()) => {
            task.mm.set_trap_context(trap_context);
            0
        }
        Err(_) => -EINVAL,
    }
}
