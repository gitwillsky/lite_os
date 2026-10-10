use crate::{
    file::PreparedFileDescription,
    fs::{MemFile, O_RDWR},
    memory::UserAccessError,
    task::current_task,
};

use super::errno;

const MFD_CLOEXEC: u32 = 0x0001;
const MFD_ALLOW_SEALING: u32 = 0x0002;

/// 创建 Linux tmpfs-backed anonymous regular file descriptor。
///
/// # Parameters
///
/// - `name`: 最多 249 bytes 的 NUL-terminated diagnostic name。
/// - `flags`: `MFD_CLOEXEC|MFD_ALLOW_SEALING` 子集。
///
/// # Returns
///
/// 新 descriptor 或负 Linux errno。
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
    let reservation = match task.fd_prepare_open(flags & MFD_CLOEXEC != 0) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    let prepared = match PreparedFileDescription::try_new() {
        Ok(prepared) => prepared,
        Err(error) => return super::fs::ferr(error),
    };
    let file = match MemFile::create_opened(
        name,
        flags & MFD_ALLOW_SEALING != 0,
        &task.access_identity(true),
    ) {
        Ok(file) => file,
        Err(_) => return -errno::ENOMEM,
    };
    let ofd = match prepared.prepare_named(file, O_RDWR) {
        Ok(prepared) => prepared.finish(crate::fs::InodeOpen::Storage),
        Err(error) => return super::fs::ferr(error),
    };
    reservation.publish(ofd) as isize
}
