//! 标准 proc fdinfo 文本 codec；file table/OFD/backend 只提供其唯一 owner 的快照。
use super::{FallibleBytes, FileSystemError, OpenedFile, chardev::DeviceFile};
use alloc::vec::Vec;
use core::fmt::Write;

/// 输出原生 pos/flags/mnt_id/ino 及实际设备 fops 字段；没有独立缓存或 Task 反查。
/// # Errors
/// inode metadata 或 formatter allocation 失败保留原错误，不输出伪成功。
pub(crate) fn format_file_information(
    opened: &OpenedFile,
    position: u64,
    flags: u32,
    backend: Option<&dyn DeviceFile>,
    view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    let mut output = FallibleBytes(Vec::new());
    write!(
        output,
        "pos:\t{position}\nflags:\t{flags:08o}\nmnt_id:\t{}\nino:\t{}\n",
        opened.mount_id(),
        opened.inode().metadata()?.inode
    )
    .map_err(|_| FileSystemError::OutOfMemory)?;
    if let Some(backend) = backend {
        backend
            .write_file_information(view, &mut output)
            .map_err(|_| FileSystemError::OutOfMemory)?;
    }
    Ok(output.0)
}
