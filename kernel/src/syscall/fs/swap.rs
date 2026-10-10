//! 标准 Linux swapon224/swapoff225；权限、pathname 与 errno 由 syscall owner 编解码。

use super::pathname::{ferr, path};
use crate::{
    fs::{self, vfs},
    memory::swap::{self, SwapError},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

fn swap_error(error: SwapError) -> isize {
    -match error {
        SwapError::Invalid | SwapError::NotFound => errno::EINVAL,
        SwapError::Busy => errno::EBUSY,
        SwapError::OutOfMemory => errno::ENOMEM,
        SwapError::Io => errno::EIO,
        SwapError::NoSpace => errno::EPERM,
        SwapError::Interrupted => errno::EINTR,
    }
}

fn source(task: &TaskControlBlock, name: &[u8]) -> Result<fs::SwapSource, isize> {
    let start = task.lookup_origin();
    vfs()
        .open_swap_at(start, name, &task.access_identity(true))
        .map_err(ferr)
}

/// Root/CAP_SYS_ADMIN projection 下启用 v1 block/file swap；filesystem lease 冻结真实物理 extent。
pub(crate) fn sys_swapon(address: usize, flags: u32) -> isize {
    let task = current_task().expect("swapon requires current task");
    if !task.has_initial_capability(syscall_abi::capability::Capability::SYS_ADMIN) {
        return -errno::EPERM;
    }
    if !swap::valid_flags(flags) {
        return -errno::EINVAL;
    }
    let name = match path(&task, address as *const u8) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let opened = match source(&task, &name) {
        Ok(opened) => opened,
        Err(error) => return error,
    };
    let storage = match fs::prepare_swap_storage(opened) {
        Ok(storage) => storage,
        Err(error) => return ferr(error),
    };
    swap::activate(storage, &name, flags).map_or_else(swap_error, |()| 0)
}

/// 预分配所有 mm wait preparation 后才开始 drain；OOM/I/O 保持剩余槽与原 storage lease。
pub(crate) fn sys_swapoff(address: usize) -> isize {
    let task = current_task().expect("swapoff requires current task");
    if !task.has_initial_capability(syscall_abi::capability::Capability::SYS_ADMIN) {
        return -errno::EPERM;
    }
    let name = match path(&task, address as *const u8) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let opened = match source(&task, &name) {
        Ok(opened) => opened,
        Err(error) => return error,
    };
    let identity = match opened.identity() {
        Ok(identity) => identity,
        Err(error) => return ferr(error),
    };
    let mut wait = match crate::sync::TaskMutexWaitPreparation::prepare() {
        Ok(wait) => wait,
        Err(_) => return -errno::ENOMEM,
    };
    swap::deactivate(identity, &mut wait).map_or_else(swap_error, |()| 0)
}
