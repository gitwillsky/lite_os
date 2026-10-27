//! asm-generic ptrace syscall adapter；内核 raw PEEK 将 word 复制到 data，不直接返回 word。
use crate::task::{self, PtraceError, PtraceRequest};
use syscall_abi::{errno, ptrace::*};

fn error(error: PtraceError) -> isize {
    -match error {
        PtraceError::NotFound => errno::ESRCH,
        PtraceError::Permission => errno::EPERM,
        PtraceError::Interrupted => errno::EINTR,
        PtraceError::OutOfMemory => errno::ENOMEM,
        PtraceError::Invalid => errno::EINVAL,
        PtraceError::Io => errno::EIO,
    }
}

/// 解码 Linux ptrace 的 request/native pid、iovec 与 siginfo。
///
/// # Parameters
/// - `request`: native long request number。
/// - `pid`: native int TID。
/// - `address`: 请求相关的用户地址或 ELF note。
/// - `data`: 请求相关的用户输出/输入地址或 signal number。
///
/// # Returns
/// Linux 返回值或负 errno；不支持的 request 返回 EIO。
pub(super) fn sys_ptrace(request: usize, pid: i32, address: usize, data: usize) -> isize {
    if request == PTRACE_TRACEME {
        return task::ptrace_attach(None).map_or_else(error, |v| v as isize);
    }
    if pid <= 0 {
        return -errno::ESRCH;
    }
    if request == PTRACE_ATTACH {
        return task::ptrace_attach(Some(pid as usize)).map_or_else(error, |v| v as isize);
    }
    let target = match task::ptrace_target(pid as usize, request != PTRACE_KILL) {
        Ok(target) => target,
        Err(failure) => return error(failure),
    };
    let caller = task::current_task().expect("ptrace requires caller");
    let result = match request {
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            match task::ptrace_request(&target, PtraceRequest::ReadWord(address)) {
                Ok(value) => {
                    return if caller.copy_to_user(data, &value.to_ne_bytes()).is_ok() {
                        0
                    } else {
                        -errno::EFAULT
                    };
                }
                Err(failure) => return error(failure),
            }
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            task::ptrace_request(&target, PtraceRequest::WriteWord(address, data))
        }
        PTRACE_CONT | PTRACE_DETACH => task::ptrace_request(
            &target,
            PtraceRequest::Resume {
                signal: data,
                detach: request == PTRACE_DETACH,
            },
        ),
        PTRACE_KILL => task::ptrace_request(&target, PtraceRequest::Kill),
        PTRACE_GETSIGINFO => {
            let mut bytes = [0u8; 128];
            if let Err(failure) =
                task::ptrace_request(&target, PtraceRequest::GetSignalInfo(&mut bytes))
            {
                return error(failure);
            }
            return if caller.copy_to_user(data, &bytes).is_ok() {
                0
            } else {
                -errno::EFAULT
            };
        }
        PTRACE_SETSIGINFO => {
            let mut bytes = [0u8; 48];
            if caller.copy_from_user(data, &mut bytes).is_err() {
                return -errno::EFAULT;
            }
            task::ptrace_request(
                &target,
                PtraceRequest::SetSignalInfo {
                    signal: i32::from_ne_bytes(bytes[..4].try_into().unwrap()) as usize,
                    info: task::PendingSignal::from_user_native(&bytes),
                },
            )
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            let mut iovec = [0u8; 16];
            if caller.copy_from_user(data, &mut iovec).is_err() {
                return -errno::EFAULT;
            }
            let base = usize::from_ne_bytes(iovec[..8].try_into().unwrap());
            let length = usize::from_ne_bytes(iovec[8..].try_into().unwrap());
            if !length.is_multiple_of(8) {
                return -errno::EINVAL;
            }
            let mut bytes = [0u8; 272];
            let length = length.min(bytes.len());
            // 1. 由实际 architecture codec 校验 note/前缀并裁剪 native 长度；RV64 不能读取多余的 A64 尾部。
            let count = match task::ptrace_request(
                &target,
                PtraceRequest::GetRegisters(address, &mut bytes[..length]),
            ) {
                Ok(count) => count,
                Err(failure) => return error(failure),
            };
            // 2. SET 仅从用户读取实际长度；user-copy 后复核同一 attachment/stopped relation。
            if request == PTRACE_SETREGSET {
                if caller.copy_from_user(base, &mut bytes[..count]).is_err() {
                    return -errno::EFAULT;
                }
                if let Err(failure) = task::ptrace_request(
                    &target,
                    PtraceRequest::SetRegisters(address, &bytes[..count]),
                ) {
                    return error(failure);
                }
            }
            if request == PTRACE_GETREGSET && caller.copy_to_user(base, &bytes[..count]).is_err() {
                return -errno::EFAULT;
            }
            return if caller.copy_to_user(data + 8, &count.to_ne_bytes()).is_ok() {
                0
            } else {
                -errno::EFAULT
            };
        }
        _ => return -errno::EIO,
    };
    result.map_or_else(error, |value| value as isize)
}
