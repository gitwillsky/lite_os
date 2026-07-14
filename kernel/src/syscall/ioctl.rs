use crate::{
    fs::{CharacterDevice, O_NONBLOCK, OpenFileKind},
    task::current_task,
};

const FIONBIO: usize = 0x5421;

use super::drm::drm_ioctl;
use super::input::input_ioctl;
use super::{errno, socket::socket_ioctl, tty::tty_ioctl};

/// @description 按 OFD backend 分发 Linux ioctl；TTY 与 socket policy 留在各自 ABI module。
///
/// @param fd 目标 descriptor。
/// @param request Linux ioctl request number。
/// @param argument request-specific scalar 或 userspace pointer。
/// @return backend handler 结果；fd、backend 或 request 不支持时返回负 errno。
pub(crate) fn sys_ioctl(fd: usize, request: usize, argument: usize) -> isize {
    // Linux syscall entry 把 ioctl cmd 解释为 unsigned int；musl 的 C prototype 使用 int，
    // RV64 会把 bit31=1 的 _IOWR 常量符号扩展到 XLEN。缺失归一化时所有双向 DRM
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
        OpenFileKind::Character(CharacterDevice::Terminal { terminal, .. }) => {
            tty_ioctl(&task, terminal, request, argument)
        }
        OpenFileKind::Character(CharacterDevice::Drm(file)) => {
            drm_ioctl(&task, file, request, argument)
        }
        OpenFileKind::Character(CharacterDevice::Input { file, .. }) => {
            input_ioctl(&task, file, request, argument)
        }
        OpenFileKind::Socket(socket) => socket_ioctl(&task, socket, request, argument),
        _ => -errno::ENOTTY,
    }
}
