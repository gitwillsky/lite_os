use crate::{file::OpenFileKind, fs::O_NONBLOCK, task::current_task};

const FIONBIO: usize = 0x5421;

use super::{errno, socket::socket_ioctl};

/// 按 OFD backend 分发 Linux ioctl；TTY 与 socket policy 留在各自 ABI module。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `request`: Linux ioctl request number。
/// - `argument`: request-specific scalar 或 userspace pointer。
///
/// # Returns
///
/// backend handler 结果；fd、backend 或 request 不支持时返回负 errno。
pub(crate) fn sys_ioctl(fd: usize, request: usize, argument: usize) -> isize {
    // Linux syscall entry 把 ioctl cmd 解释为 unsigned int；musl 的 C prototype 使用 int，
    // LP64 用户态会把 bit31=1 的 int _IOWR 常量符号扩展到 64-bit register。缺失归一化时所有双向 DRM
    // request 都会与 32-bit UAPI 常量失配并错误返回 ENOTTY。
    let request = request as u32 as usize;
    let task = current_task().expect("ioctl requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if request == FIONBIO {
        if argument == 0 {
            return -errno::EFAULT;
        }
        let mut bytes = [0u8; 4];
        if task.copy_from_user(argument, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let mut flags = ofd.flags.lock();
        if i32::from_ne_bytes(bytes) == 0 {
            *flags &= !O_NONBLOCK;
        } else {
            *flags |= O_NONBLOCK;
        }
        return 0;
    }
    match &ofd.kind {
        OpenFileKind::Device(file) => {
            super::device::ioctl_device(&task, &ofd, file.as_ref(), request, argument)
        }
        OpenFileKind::Pipe(endpoint) => pipe_ioctl(&task, &endpoint.pipe(), request, argument),
        OpenFileKind::Inode(opened) => {
            super::device::ioctl_inode(&task, &ofd, opened.inode().as_ref(), request, argument)
        }
        OpenFileKind::Socket(socket) => socket_ioctl(&task, socket, request, argument),
        _ => -errno::ENOTTY,
    }
}

/// 匿名管道的 ioctl：只有 `FIONREAD`。
fn pipe_ioctl(
    task: &crate::task::TaskControlBlock,
    pipe: &alloc::sync::Arc<crate::ipc::Pipe>,
    request: usize,
    argument: usize,
) -> isize {
    const FIONREAD: usize = 0x541b;
    if request != FIONREAD {
        return -errno::ENOTTY;
    }
    let buffered = i32::try_from(pipe.buffered_bytes()).unwrap_or(i32::MAX);
    match task.copy_to_user(argument, &buffered.to_ne_bytes()) {
        Ok(()) => 0,
        Err(_) => -errno::EFAULT,
    }
}
