use alloc::vec::Vec;

use crate::fs::{FileSystemError, InodeType, vfs::vfs};

/// @description 从启动文件系统读取可执行文件时的可观察失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramLoadError {
    /// 无法为完整 file image 分配 kernel 缓冲区。
    OutOfMemory,
    /// VFS 路径解析或块读取失败。
    FileSystem(FileSystemError),
    /// 最终 inode 不是普通文件。
    NotRegularFile,
    /// 普通文件没有任何 execute mode bit。
    NotExecutable,
}

/// @description 按 Linux 字节路径从唯一根 VFS 完整读入程序映像。
///
/// @param path 不含 NUL 的绝对路径字节。
/// @return 成功返回完整 file bytes。
/// @errors 路径、inode 类型、I/O 或 short read 失败均保留为明确错误。
pub fn load_program_from_fs(path: &[u8]) -> Result<Vec<u8>, ProgramLoadError> {
    let inode = vfs().open(path).map_err(ProgramLoadError::FileSystem)?;
    if inode.inode_type() != InodeType::File {
        return Err(ProgramLoadError::NotRegularFile);
    }
    if !inode.is_executable() {
        return Err(ProgramLoadError::NotExecutable);
    }

    let size = usize::try_from(inode.size())
        .map_err(|_| ProgramLoadError::FileSystem(FileSystemError::IoError))?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| ProgramLoadError::OutOfMemory)?;
    buffer.resize(size, 0);
    let bytes_read = inode
        .read_at(0, &mut buffer)
        .map_err(ProgramLoadError::FileSystem)?;
    if bytes_read != size {
        return Err(ProgramLoadError::FileSystem(FileSystemError::IoError));
    }
    Ok(buffer)
}
