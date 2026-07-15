use crate::{
    fs::{O_CLOEXEC, O_NONBLOCK, OpenFileDescription},
    ipc::EventFd,
    syscall::errno,
    task::{create_notification_endpoints, current_task},
};

const EFD_SEMAPHORE: u32 = 1;

/// @description 创建 Linux eventfd counter OFD，并按 flags 原子发布 descriptor。
/// @param initial 初始 32-bit counter。
/// @param flags 只接受 EFD_SEMAPHORE/EFD_NONBLOCK/EFD_CLOEXEC。
/// @return 新 fd；flags、内存或 fd limit 失败返回负 errno。
pub(crate) fn sys_eventfd2(initial: u32, flags: u32) -> isize {
    if flags & !(EFD_SEMAPHORE | O_NONBLOCK | O_CLOEXEC) != 0 {
        return -errno::EINVAL;
    }
    let read_pair = match create_notification_endpoints() {
        Ok(pair) => pair,
        Err(()) => return -errno::ENOMEM,
    };
    let write_pair = match create_notification_endpoints() {
        Ok(pair) => pair,
        Err(()) => return -errno::ENOMEM,
    };
    let event = match EventFd::new(
        u64::from(initial),
        flags & EFD_SEMAPHORE != 0,
        read_pair,
        write_pair,
    ) {
        Ok(event) => event,
        Err(()) => return -errno::ENOMEM,
    };
    let task = current_task().expect("eventfd2 requires current task");
    let ofd = match OpenFileDescription::event_fd(event, flags & O_NONBLOCK) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    task.fd_allocate(ofd, flags & O_CLOEXEC != 0)
        .map_or_else(super::file_descriptor_error, |fd| fd as isize)
}
