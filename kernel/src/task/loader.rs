use alloc::sync::Arc;

use crate::{
    fs::{FileSystemError, Inode, InodeType, vfs},
    memory::{
        ExecutableImage, ExecutableParseError, ExecutableSource, parse_interpreter_elf,
        parse_main_elf,
    },
};

/// @description executable pathname 解析、权限检查与 ELF mapping plan 构造失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgramLoadError {
    /// 有界 ELF metadata 或 mapping plan 分配失败。
    OutOfMemory,
    /// VFS pathname 解析或 executable source 读取失败。
    FileSystem(FileSystemError),
    /// 最终 inode 不是普通文件。
    NotRegularFile,
    /// 普通文件没有任何 execute mode bit。
    NotExecutable,
    /// ELF header、program header 或 PT_INTERP 不满足 RV64 契约。
    InvalidElf,
}

struct InodeExecutableSource {
    inode: Arc<dyn Inode>,
    length: usize,
}

impl ExecutableSource for InodeExecutableSource {
    fn len(&self) -> usize {
        self.length
    }

    fn read_exact_at(&self, offset: usize, buffer: &mut [u8]) -> Result<(), ()> {
        self.inode
            .read_at(offset as u64, buffer)
            .ok()
            .filter(|read| *read == buffer.len())
            .map(|_| ())
            .ok_or(())
    }
}

/// @description 从 VFS pathname 构造主程序与动态解释器的唯一 executable image。
///
/// @param path 不含 NUL 的绝对或相对 pathname bytes。
/// @return 可直接交给 MemorySet transaction 的映射计划。
/// @errors 返回 pathname、inode permission、资源或 ELF validation 错误。
pub(crate) fn load_executable_from_fs(path: &[u8]) -> Result<ExecutableImage, ProgramLoadError> {
    let inode = vfs().open(path).map_err(ProgramLoadError::FileSystem)?;
    load_executable_from_inode(inode)
}

/// @description 从已解析的主程序 inode 构造唯一 executable image。
///
/// @param inode execve 最终解析得到的 inode。
/// @return 主程序与可选动态解释器的映射计划。
/// @errors 返回 inode permission、资源、读取或 ELF validation 错误。
pub(crate) fn load_executable_from_inode(
    inode: Arc<dyn Inode>,
) -> Result<ExecutableImage, ProgramLoadError> {
    let main_source = source(inode)?;
    let (main, interpreter_path) = parse_main_elf(main_source).map_err(parse_error)?;
    let interpreter = interpreter_path
        .map(|path| {
            let inode = vfs().open(&path).map_err(ProgramLoadError::FileSystem)?;
            parse_interpreter_elf(source(inode)?).map_err(parse_error)
        })
        .transpose()?;
    Ok(ExecutableImage::new(main, interpreter))
}

fn source(inode: Arc<dyn Inode>) -> Result<Arc<dyn ExecutableSource>, ProgramLoadError> {
    if inode.inode_type() != InodeType::File {
        return Err(ProgramLoadError::NotRegularFile);
    }
    if !inode.is_executable() {
        return Err(ProgramLoadError::NotExecutable);
    }
    let length = usize::try_from(inode.size())
        .map_err(|_| ProgramLoadError::FileSystem(FileSystemError::IoError))?;
    Ok(Arc::new(InodeExecutableSource { inode, length }))
}

fn parse_error(error: ExecutableParseError) -> ProgramLoadError {
    match error {
        ExecutableParseError::OutOfMemory => ProgramLoadError::OutOfMemory,
        ExecutableParseError::InvalidElf => ProgramLoadError::InvalidElf,
        ExecutableParseError::Io => ProgramLoadError::FileSystem(FileSystemError::IoError),
    }
}
