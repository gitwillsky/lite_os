use crate::{
    fs::{CharacterDevice, OpenFileKind},
    task::current_task,
};

use super::{errno, socket::socket_ioctl, tty::tty_ioctl};

/// @description 按 OFD backend 分发 Linux ioctl；TTY 与 socket policy 留在各自 ABI module。
///
/// @param fd 目标 descriptor。
/// @param request Linux ioctl request number。
/// @param argument request-specific scalar 或 userspace pointer。
/// @return backend handler 结果；fd、backend 或 request 不支持时返回负 errno。
pub(crate) fn sys_ioctl(fd: usize, request: usize, argument: usize) -> isize {
    let task = current_task().expect("ioctl requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    match &ofd.kind {
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) => {
            tty_ioctl(&task, terminal, request, argument)
        }
        OpenFileKind::Socket(socket) => socket_ioctl(&task, socket, request, argument),
        _ => -errno::ENOTTY,
    }
}
