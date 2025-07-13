use crate::memory::page_table::translated_byte_buffer;
use crate::task::{
    current_task, current_user_token,
    signal::{
        Signal, SignalAction, SignalDelivery, SignalDisposition, SignalError, SignalSet,
        send_signal_to_process,
    },
};

/// 信号掩码操作常量
pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

/// 特殊信号处理器值
pub const SIG_DFL: usize = 0; // 默认动作
pub const SIG_IGN: usize = 1; // 忽略信号

/// kill系统调用 - 向指定进程发送信号
pub fn sys_kill(pid: usize, sig: u32) -> isize {
    // 验证信号号是否有效
    if let Some(signal) = Signal::from_u8(sig as u8) {
        match send_signal_to_process(pid, signal) {
            Ok(()) => 0,
            Err(SignalError::ProcessNotFound) => -1, // ESRCH
            Err(SignalError::PermissionDenied) => -1, // EPERM
            Err(_) => -1,
        }
    } else {
        -1 // EINVAL - Invalid signal
    }
}

/// signal系统调用 - 设置信号处理函数
pub fn sys_signal(sig: u32, handler: usize) -> isize {
    if let Some(signal) = Signal::from_u8(sig as u8) {
        // 不能捕获SIGKILL和SIGSTOP
        if signal.is_uncatchable() {
            return -1; // EINVAL
        }

        if let Some(task) = current_task() {
            let inner = task.inner_exclusive_access();

            // 获取当前的信号处理器
            let old_handler = inner.get_signal_handler(signal);
            let old_handler_addr = match old_handler.action {
                SignalAction::Handler(addr) => addr as isize,
                SignalAction::Ignore => SIG_IGN as isize,
                _ => SIG_DFL as isize,
            };

            // 设置新的信号处理器
            let new_action = match handler {
                SIG_DFL => signal.default_action(),
                SIG_IGN => SignalAction::Ignore,
                addr => SignalAction::Handler(addr),
            };

            let disposition = SignalDisposition {
                action: new_action,
                mask: SignalSet::new(),
                flags: 0,
            };

            inner.set_signal_handler(signal, disposition);
            drop(inner);

            old_handler_addr
        } else {
            -1
        }
    } else {
        -1 // EINVAL
    }
}

/// sigaction结构体
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigAction {
    pub sa_handler: usize,
    pub sa_mask: u64,
    pub sa_flags: u32,
    pub sa_restorer: usize,
}

/// sigaction系统调用 - 更高级的信号处理设置
pub fn sys_sigaction(sig: u32, act: *const SigAction, oldact: *mut SigAction) -> isize {
    if let Some(signal) = Signal::from_u8(sig as u8) {
        // 不能捕获SIGKILL和SIGSTOP
        if signal.is_uncatchable() {
            return -1; // EINVAL
        }

        if let Some(task) = current_task() {
            let token = current_user_token();

            // 获取当前的信号处理器
            let old_handler = task.inner_exclusive_access().get_signal_handler(signal);

            // 如果oldact不为空，返回旧的信号处理器
            if !oldact.is_null() {
                let old_sigaction = SigAction {
                    sa_handler: match old_handler.action {
                        SignalAction::Handler(addr) => addr,
                        SignalAction::Ignore => SIG_IGN,
                        _ => SIG_DFL,
                    },
                    sa_mask: old_handler.mask.to_raw(),
                    sa_flags: old_handler.flags,
                    sa_restorer: 0,
                };

                // 将旧的sigaction写入用户空间
                let old_sigaction_bytes = unsafe {
                    core::slice::from_raw_parts(
                        &old_sigaction as *const _ as *const u8,
                        core::mem::size_of::<SigAction>(),
                    )
                };

                let mut buffers = translated_byte_buffer(
                    token,
                    oldact as *mut u8,
                    core::mem::size_of::<SigAction>(),
                );
                if !buffers.is_empty() && buffers[0].len() >= core::mem::size_of::<SigAction>() {
                    buffers[0].copy_from_slice(old_sigaction_bytes);
                } else {
                    return -1; // EFAULT
                }
            }

            // 如果act不为空，设置新的信号处理器
            if !act.is_null() {
                // 从用户空间读取新的sigaction
                let buffers = translated_byte_buffer(
                    token,
                    act as *const u8,
                    core::mem::size_of::<SigAction>(),
                );
                if !buffers.is_empty() && buffers[0].len() >= core::mem::size_of::<SigAction>() {
                    let new_sigaction = unsafe { *(buffers[0].as_ptr() as *const SigAction) };

                    let new_action = match new_sigaction.sa_handler {
                        SIG_DFL => signal.default_action(),
                        SIG_IGN => SignalAction::Ignore,
                        addr => SignalAction::Handler(addr),
                    };

                    let disposition = SignalDisposition {
                        action: new_action,
                        mask: SignalSet::from_raw(new_sigaction.sa_mask),
                        flags: new_sigaction.sa_flags,
                    };

                    task.inner_exclusive_access().set_signal_handler(signal, disposition);
                } else {
                    return -1; // EFAULT
                }
            }

            0
        } else {
            -1
        }
    } else {
        -1 // EINVAL
    }
}

/// sigprocmask系统调用 - 设置信号掩码
pub fn sys_sigprocmask(how: i32, set: *const u64, oldset: *mut u64) -> isize {
    if let Some(task) = current_task() {
        let token = current_user_token();

        // 获取当前信号掩码
        let old_mask = task.inner_exclusive_access().get_signal_mask();

        // 如果oldset不为空，返回旧的信号掩码
        if !oldset.is_null() {
            let old_mask_raw = old_mask.to_raw();
            let old_mask_bytes = unsafe {
                core::slice::from_raw_parts(
                    &old_mask_raw as *const _ as *const u8,
                    core::mem::size_of::<u64>(),
                )
            };

            let mut buffers =
                translated_byte_buffer(token, oldset as *mut u8, core::mem::size_of::<u64>());
            if !buffers.is_empty() && buffers[0].len() >= core::mem::size_of::<u64>() {
                buffers[0].copy_from_slice(old_mask_bytes);
            } else {
                return -1; // EFAULT
            }
        }

        // 如果set不为空，设置新的信号掩码
        if !set.is_null() {
            let buffers =
                translated_byte_buffer(token, set as *const u8, core::mem::size_of::<u64>());
            if !buffers.is_empty() && buffers[0].len() >= core::mem::size_of::<u64>() {
                let new_mask_raw = unsafe { *(buffers[0].as_ptr() as *const u64) };
                let new_mask = SignalSet::from_raw(new_mask_raw);
                let inner = task.inner_exclusive_access();

                match how {
                    SIG_BLOCK => {
                        let combined_mask = old_mask.union(&new_mask);
                        inner.set_signal_mask(combined_mask);
                    }
                    SIG_UNBLOCK => {
                        let unblocked_mask = old_mask.difference(&new_mask);
                        inner.set_signal_mask(unblocked_mask);
                    }
                    SIG_SETMASK => {
                        inner.set_signal_mask(new_mask);
                    }
                    _ => {
                        return -1; // EINVAL
                    }
                }
            } else {
                return -1; // EFAULT
            }
        }

        0
    } else {
        -1
    }
}

/// sigreturn系统调用 - 从信号处理函数返回
pub fn sys_sigreturn() -> isize {
    if let Some(task) = current_task() {
        let trap_cx = task.inner_exclusive_access().get_trap_cx();

        // 调用信号处理引擎的sigreturn方法
        let success = SignalDelivery::sigreturn(&task, trap_cx);

        if success {
            0 // 成功返回
        } else {
            -1 // 失败
        }
    } else {
        -1
    }
}

/// pause系统调用 - 暂停进程直到收到信号
pub fn sys_pause() -> isize {
    if let Some(task) = current_task() {
        let mut inner = task.inner_exclusive_access();

        // 检查是否有待处理的信号
        if inner.has_pending_signals() {
            // 如果有信号待处理，不暂停
            drop(inner);
            return -1; // EINTR
        }

        // 设置进程为睡眠状态
        inner.task_status = crate::task::TaskStatus::Sleeping;
        drop(inner);

        // 让出CPU
        crate::task::suspend_current_and_run_next();

        // 当进程被唤醒时（通常是因为收到信号），返回-1
        -1 // EINTR
    } else {
        -1
    }
}

/// alarm系统调用 - 设置定时器信号
pub fn sys_alarm(seconds: u32) -> isize {
    // 简化实现：直接返回0
    // 在实际实现中，这应该设置一个定时器，在指定时间后发送SIGALRM信号
    0
}
