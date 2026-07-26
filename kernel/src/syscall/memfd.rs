use crate::{
    fs::{MemFile, O_RDWR, OpenFileDescription},
    memory::UserAccessError,
    task::current_task,
};

use super::errno;

const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;

/// @description 创建 Linux tmpfs-backed anonymous regular file descriptor。
/// @param name 最多 249 bytes 的 NUL-terminated diagnostic name。
/// @param flags `MFD_CLOEXEC|MFD_ALLOW_SEALING` 子集。
/// @return 新 descriptor 或负 Linux errno。
pub(crate) fn sys_memfd_create(name: usize, flags: u32) -> isize {
    if name == 0 || flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING) != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("memfd_create requires current task");
    let name = match task.copy_user_c_string(name, 250) {
        Ok(name) => name,
        Err(UserAccessError::Unterminated) => return -errno::EINVAL,
        Err(UserAccessError::OutOfMemory) => return -errno::ENOMEM,
        Err(UserAccessError::Fault | UserAccessError::Overflow) => return -errno::EFAULT,
    };
    let file = match MemFile::new(name, flags & MFD_ALLOW_SEALING != 0) {
        Ok(file) => file,
        Err(_) => return -errno::ENOMEM,
    };
    let ofd = match OpenFileDescription::mem_file(file, O_RDWR) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    task.fd_allocate(ofd, flags & MFD_CLOEXEC != 0)
        .map_or_else(super::file_descriptor_error, |fd| fd as isize)
}
