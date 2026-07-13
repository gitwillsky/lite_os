use crate::task::{ResourceLimit, ResourceLimitError, current_task, process_resource_limit};

use super::errno;

/// @description 实现 Linux/riscv64 prlimit64 的 Process owner、权限与 copyout 顺序。
pub(crate) fn sys_prlimit64(
    pid: usize,
    resource: usize,
    replacement: usize,
    previous: usize,
) -> isize {
    let task = current_task().expect("prlimit64 requires a current task");
    let replacement = if replacement == 0 {
        None
    } else {
        let mut bytes = [0u8; 16];
        if task.copy_from_user(replacement, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        Some(ResourceLimit {
            soft: u64::from_ne_bytes(bytes[..8].try_into().unwrap()),
            hard: u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
        })
    };
    let old = match process_resource_limit(pid, resource, replacement) {
        Ok(limit) => limit,
        Err(ResourceLimitError::NotFound) => return -errno::ESRCH,
        Err(ResourceLimitError::PermissionDenied) => return -errno::EPERM,
        Err(ResourceLimitError::InvalidResource | ResourceLimitError::InvalidLimit) => {
            return -errno::EINVAL;
        }
    };
    if previous != 0 {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&old.soft.to_ne_bytes());
        bytes[8..].copy_from_slice(&old.hard.to_ne_bytes());
        if task.copy_to_user(previous, &bytes).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}
