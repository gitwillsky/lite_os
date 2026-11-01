use super::errno;
use crate::{
    execution::task::current_task,
    file::{OpenFileDescription, OpenFileKind},
    fs::{O_CLOEXEC, O_NONBLOCK},
};

/// Linux signalfd4：创建 reader-relative OFD 或替换既有 OFD 的消费 mask。
/// mask 必须是原生 8-byte sigset；更新不修改 status/descriptor flags。
pub(crate) fn sys_signalfd4(fd: i32, pointer: usize, size: usize, flags: u32) -> isize {
    if size != 8 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("signalfd4 requires current task");
    let mut bytes = [0u8; 8];
    if task.copy_from_user(pointer, &mut bytes).is_err() {
        return -errno::EFAULT;
    }
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    let mask = u64::from_ne_bytes(bytes);
    if fd == -1 {
        let ofd = match OpenFileDescription::signal_fd(mask, flags & O_NONBLOCK) {
            Ok(ofd) => ofd,
            Err(()) => return -errno::ENOMEM,
        };
        task.fd_allocate(ofd, 0, flags & O_CLOEXEC != 0)
            .map_or_else(super::file_descriptor_error, |fd| fd as isize)
    } else {
        let Some(ofd) = usize::try_from(fd).ok().and_then(|fd| task.fd_get(fd)) else {
            return -errno::EBADF;
        };
        let OpenFileKind::SignalFd(signal) = &ofd.kind else {
            return -errno::EINVAL;
        };
        signal.replace_mask(mask);
        crate::execution::task::notify_signal_waiters(&task);
        fd as isize
    }
}
