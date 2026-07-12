use alloc::{sync::Arc, vec::Vec};

use crate::fs::{FileSystemError, Inode, InodeType, vfs};
use crate::memory::ExecutableImage;

/// @description 从启动文件系统读取可执行文件时的可观察失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgramLoadError {
    /// 无法为完整 file image 分配 kernel 缓冲区。
    OutOfMemory,
    /// VFS 路径解析或块读取失败。
    FileSystem(FileSystemError),
    /// 最终 inode 不是普通文件。
    NotRegularFile,
    /// 普通文件没有任何 execute mode bit。
    NotExecutable,
    /// ELF program-header table 或 PT_INTERP pathname 非法。
    InvalidElf,
}

/// @description 读取主程序并按唯一 PT_INTERP 读取动态解释器。
pub(crate) fn load_executable_from_fs(path: &[u8]) -> Result<ExecutableImage, ProgramLoadError> {
    let main = load_program_from_fs(path)?;
    load_interpreter(main)
}

/// @description 从已解析 inode 读取主程序并构造完整 executable image。
pub(crate) fn load_executable_from_inode(
    inode: Arc<dyn Inode>,
) -> Result<ExecutableImage, ProgramLoadError> {
    let main = load_program_from_inode(inode)?;
    load_interpreter(main)
}

fn load_interpreter(main: Vec<u8>) -> Result<ExecutableImage, ProgramLoadError> {
    let elf = xmas_elf::ElfFile::new(&main).map_err(|_| ProgramLoadError::InvalidElf)?;
    let mut interpreter_path = None;
    for index in 0..elf.header.pt2.ph_count() {
        let header = elf
            .program_header(index)
            .map_err(|_| ProgramLoadError::InvalidElf)?;
        if header
            .get_type()
            .map_err(|_| ProgramLoadError::InvalidElf)?
            != xmas_elf::program::Type::Interp
        {
            continue;
        }
        if interpreter_path.is_some() {
            return Err(ProgramLoadError::InvalidElf);
        }
        let start = usize::try_from(header.offset()).map_err(|_| ProgramLoadError::InvalidElf)?;
        let size = usize::try_from(header.file_size()).map_err(|_| ProgramLoadError::InvalidElf)?;
        let bytes = main
            .get(
                start
                    ..start
                        .checked_add(size)
                        .ok_or(ProgramLoadError::InvalidElf)?,
            )
            .ok_or(ProgramLoadError::InvalidElf)?;
        let path = bytes
            .strip_suffix(&[0])
            .ok_or(ProgramLoadError::InvalidElf)?;
        if path.first() != Some(&b'/') || path.is_empty() || path.contains(&0) {
            return Err(ProgramLoadError::InvalidElf);
        }
        interpreter_path = Some(path);
    }
    let interpreter = interpreter_path.map(load_program_from_fs).transpose()?;
    Ok(ExecutableImage { main, interpreter })
}

/// @description 按 Linux 字节路径从唯一根 VFS 完整读入程序映像。
///
/// @param path 不含 NUL 的绝对路径字节。
/// @return 成功返回完整 file bytes。
/// @errors 路径、inode 类型、I/O 或 short read 失败均保留为明确错误。
pub(crate) fn load_program_from_fs(path: &[u8]) -> Result<Vec<u8>, ProgramLoadError> {
    let inode = vfs().open(path).map_err(ProgramLoadError::FileSystem)?;
    load_program_from_inode(inode)
}

/// @description 从已由 VFS 解析的 inode 完整读入可执行映像。
///
/// @param inode pathname lookup 产生且在读取期间保活的 inode。
/// @return 成功返回完整 file bytes。
/// @errors inode 类型、execute mode、内存、I/O 或 short read 失败时返回明确错误。
pub(crate) fn load_program_from_inode(inode: Arc<dyn Inode>) -> Result<Vec<u8>, ProgramLoadError> {
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
