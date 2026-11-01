//! seccomp/prctl 共用的标准 UAPI installation 与 syscall-entry enforcement。

use syscall_abi::{errno, seccomp::*, signal::SIGSYS};

use super::{SyscallOutcome, user_iovec::UserInputStaging};
use crate::task::{PendingSignal, SeccompDecision, SeccompError, TaskControlBlock, current_task};
use alloc::sync::Arc;

fn result(result: Result<Option<usize>, SeccompError>, flags: u32) -> isize {
    result.map_or_else(
        |error| match error {
            SeccompError::Busy => -errno::EBUSY,
            SeccompError::Descriptor(error) => super::file_descriptor_error(error),
            SeccompError::Invalid => -errno::EINVAL,
            SeccompError::Interrupted => super::INTERNAL_RESTART_SYS,
            SeccompError::OutOfMemory => -errno::ENOMEM,
            SeccompError::Unsynchronized(tid) => {
                if flags & FILTER_FLAG_TSYNC_ESRCH != 0 {
                    -errno::ESRCH
                } else {
                    tid as isize
                }
            }
        },
        |fd| fd.map_or(0, |fd| fd as isize),
    )
}

/// prctl STRICT 忽略 filter 参数；277 的 NULL/flags 检查由其独立 codec 执行。
pub(super) fn set_mode(mode: usize, filter: usize) -> isize {
    match mode {
        value if value == MODE_STRICT as usize => {
            result(crate::task::enable_strict_seccomp().map(|()| None), 0)
        }
        value if value == MODE_FILTER as usize => install_filter(filter, 0),
        _ => -errno::EINVAL,
    }
}

/// 返回 calling Thread 的 seccomp mode；prctl 其余参数按 Linux 忽略。
pub(super) fn get_mode() -> isize {
    current_task()
        .expect("seccomp requires current task")
        .seccomp_mode() as isize
}

fn install_filter(address: usize, flags: u32) -> isize {
    let current = current_task().expect("seccomp requires current task");
    let mut header = [0u8; 16];
    if current.copy_from_user(address, &mut header).is_err() {
        return -errno::EFAULT;
    }
    let length = u16::from_ne_bytes(header[..2].try_into().unwrap()) as usize;
    if !(1..=4096).contains(&length) {
        return -errno::EINVAL;
    }
    // NNP 或 calling credential 的 CAP_SYS_ADMIN 授权；不允许 UID 0 绕过已撤销的 capability。
    if !current.no_new_privileges()
        && !current.has_capability(syscall_abi::capability::Capability::SYS_ADMIN)
    {
        return -errno::EACCES;
    }
    let pointer = usize::from_ne_bytes(header[8..16].try_into().unwrap());
    let size = length * 8;
    let mut input = match UserInputStaging::try_new(size) {
        Ok(input) => input,
        Err(()) => return -errno::ENOMEM,
    };
    if current
        .copy_from_user_uninit(pointer, input.prepare(size))
        .is_err()
    {
        return -errno::EFAULT;
    }
    // SAFETY: 完整 copy_from_user_uninit 成功证明 prepared destination 的全部 size bytes 已初始化。
    unsafe { input.publish(size) };
    result(
        crate::task::install_seccomp_filter(input.initialized(), flags),
        flags,
    )
}

/// 标准 syscall 277；op/flags 按 unsigned int 解码。
///
/// STRICT 要求 flags=0、arguments=NULL；FILTER flags 对照固定 Linux；ACTION_AVAIL/SIZES 固定 codec。
/// 未实现 operation/flag 返回 EINVAL；copy fault、授权、program 和 OOM 使用标准 errno。
pub(crate) fn sys_seccomp(operation: u32, flags: u32, arguments: usize) -> isize {
    match operation {
        SET_MODE_STRICT if flags == 0 && arguments == 0 => set_mode(MODE_STRICT as usize, 0),
        SET_MODE_FILTER if flags & !FILTER_FLAG_MASK == 0 => {
            if flags & FILTER_FLAG_TSYNC != 0
                && flags & FILTER_FLAG_NEW_LISTENER != 0
                && flags & FILTER_FLAG_TSYNC_ESRCH == 0
                || flags & FILTER_FLAG_WAIT_KILLABLE_RECV != 0
                    && flags & FILTER_FLAG_NEW_LISTENER == 0
            {
                return -errno::EINVAL;
            }
            install_filter(arguments, flags)
        }
        GET_NOTIF_SIZES if flags == 0 => {
            let mut bytes = [0u8; 6];
            bytes[..2].copy_from_slice(&80u16.to_ne_bytes());
            bytes[2..4].copy_from_slice(&24u16.to_ne_bytes());
            bytes[4..].copy_from_slice(&64u16.to_ne_bytes());
            current_task()
                .expect("seccomp requires current task")
                .copy_to_user(arguments, &bytes)
                .map_or(-errno::EFAULT, |()| 0)
        }
        GET_ACTION_AVAIL if flags == 0 => {
            let mut bytes = [0u8; 4];
            if current_task()
                .expect("seccomp requires current task")
                .copy_from_user(arguments, &mut bytes)
                .is_err()
            {
                return -errno::EFAULT;
            }
            match u32::from_ne_bytes(bytes) {
                RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_TRACE
                | RET_USER_NOTIF | RET_LOG | RET_ALLOW => 0,
                _ => -errno::EOPNOTSUPP,
            }
        }
        _ => -errno::EINVAL,
    }
}

/// dispatcher 在 architecture decode/任何 handler 前消费领域 decision。
///
/// Allow 返回 None；拒绝返回标准 completion，TRAP 保留原寄存器；fatal consequence 不返回。
/// 所有 TCB/program Arc 在不返回的退出前释放，不滞留在已退休的 kernel stack。
pub(super) fn enforce(
    current: Arc<TaskControlBlock>,
    number: usize,
    args: [usize; 6],
) -> Option<SyscallOutcome> {
    match current.evaluate_seccomp(number, args) {
        SeccompDecision::Allow => None,
        SeccompDecision::Errno(value) => Some(SyscallOutcome::Return(-(value as isize))),
        SeccompDecision::NoObserver => Some(SyscallOutcome::Return(-errno::ENOSYS)),
        SeccompDecision::Notify {
            queue,
            data,
            killable_after_receive,
        } => match current.wait_seccomp_notification(&queue, data, killable_after_receive) {
            Ok(None) => None,
            Ok(Some(value)) => Some(SyscallOutcome::Return(value)),
            Err(SeccompError::Interrupted) => Some(SyscallOutcome::Restart(
                crate::task::SyscallRestartPolicy::WithRestartHandler,
            )),
            Err(SeccompError::OutOfMemory) => Some(SyscallOutcome::Return(-errno::ENOMEM)),
            Err(_) => unreachable!("notification has only interrupt/OOM failures"),
        },
        SeccompDecision::Trap(value) => {
            let info = PendingSignal::seccomp(
                value,
                number,
                crate::system::audit_architecture(),
                current.user_program_counter(),
            );
            current
                .queue_synchronous_fault(SIGSYS, info)
                .expect("valid forced SIGSYS");
            Some(SyscallOutcome::PreserveRegisters)
        }
        SeccompDecision::KillThread(signal) => {
            drop(current);
            crate::task::exit_current_thread_by_signal(signal)
        }
        SeccompDecision::KillProcess => {
            drop(current);
            crate::task::exit_current_group_by_signal(SIGSYS)
        }
        SeccompDecision::Log => None,
    }
}
