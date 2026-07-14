use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{
        AccessIdentity, FileSystemError, Inode, InodeMetadata, InodeType, OpenedFile, RegularFile,
        vfs,
    },
    memory::{
        ElfLoadError, ExecutableImage, ExecutableParseError, ExecutableSource, MemorySet,
        parse_interpreter_elf, parse_main_elf,
    },
};

/// @description argv/envp strings、NUL 与 pointer slots 共用的 exec byte budget。
pub(crate) const EXEC_ARGUMENT_BYTES_LIMIT: usize = 128 * 1024;

/// @description pathname、script rewrite、权限检查与 ELF mapping plan 的完整加载结果。
pub(crate) struct LoadedExecutable {
    image: ExecutableImage,
    arguments: Vec<Vec<u8>>,
    execfn: Vec<u8>,
    credentials: InodeMetadata,
}

impl LoadedExecutable {
    /// @description 从最终 ELF plan 与 rewritten argv transactionally 构造新地址空间。
    ///
    /// @param environments 已从 userspace 完整复制且不含 NUL 的 envp strings。
    /// @return 新 MemorySet、initial sp 与 entry point。
    /// @errors ELF mapping、initial stack、source I/O 或资源失败。
    pub(super) fn build_address_space(
        &self,
        environments: &[Vec<u8>],
        stack_limit: u64,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<(MemorySet, usize, usize), ElfLoadError> {
        MemorySet::from_elf(
            &self.image,
            &self.arguments,
            environments,
            &self.execfn,
            stack_limit,
            address_space_limit,
            data_limit,
        )
    }

    /// @description 返回用户传给 execve 的原始 pathname，用于 AT_EXECFN 与 process comm。
    ///
    /// @return 不含 NUL 的 immutable pathname bytes。
    pub(super) fn execfn(&self) -> &[u8] {
        &self.execfn
    }

    pub(super) fn credential_metadata(&self) -> InodeMetadata {
        self.credentials
    }
}

/// @description executable pathname 解析、权限检查与 mapping plan 构造失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgramLoadError {
    /// 有界 probe、ELF metadata、argv rewrite 或 mapping plan 分配失败。
    OutOfMemory,
    /// VFS pathname 解析或 executable source 读取失败。
    FileSystem(FileSystemError),
    /// 最终 inode 不是普通文件。
    NotRegularFile,
    /// 普通文件没有任何 execute mode bit。
    NotExecutable,
    /// ELF header、program header 或 script interpreter line 不满足契约。
    InvalidExecutable,
    /// script interpreter rewrite 超过 Linux 固定上限。
    InterpreterLoop,
    /// script rewrite 后 argv/envp 超过 exec argument byte limit。
    ArgumentListTooLong,
}

struct InodeExecutableSource {
    file: RegularFile,
    length: usize,
}

impl ExecutableSource for InodeExecutableSource {
    fn len(&self) -> usize {
        self.length
    }

    fn read_exact_at(&self, offset: usize, buffer: &mut [u8]) -> Result<(), ()> {
        self.file
            .read(offset as u64, buffer)
            .ok()
            .filter(|read| read.bytes == buffer.len())
            .map(|_| ())
            .ok_or(())
    }
}

struct ScriptHeader {
    interpreter: Vec<u8>,
    argument: Option<Vec<u8>>,
}

struct ScriptRewrite {
    path: Vec<u8>,
    arguments: Vec<Vec<u8>>,
    argument_bytes: usize,
}

/// @description 解析 pathname，按 Linux binfmt_script 规则重写 argv，最终构造唯一 ELF image。
///
/// @param working_directory relative pathname 与 relative script interpreter 的解析起点。
/// @param path 用户传入且不含 NUL 的原始 exec pathname；成功后成为 AT_EXECFN。
/// @param arguments 已从 userspace 完整复制的 argv。
/// @param argument_bytes 当前 argv/envp 的受限 byte accounting。
/// @return 最终 ELF image、重写后的 argv 与原始 AT_EXECFN pathname。
/// @errors 返回 pathname、权限、资源、argument limit、interpreter loop 或格式错误。
pub(crate) fn load_executable(
    working_directory: Arc<OpenedFile>,
    path: Vec<u8>,
    mut arguments: Vec<Vec<u8>>,
    mut argument_bytes: usize,
    identity: &AccessIdentity,
) -> Result<LoadedExecutable, ProgramLoadError> {
    const MAX_SCRIPT_REWRITES: usize = 5;

    normalize_arguments(&mut arguments, &mut argument_bytes)?;
    let execfn = copy_bytes(&path)?;
    let mut current_path = path;
    for rewrite_count in 0..=MAX_SCRIPT_REWRITES {
        let inode = vfs()
            .open_at(Some(working_directory.clone()), &current_path, identity)
            .map_err(ProgramLoadError::FileSystem)?;
        let metadata = inode.metadata().map_err(ProgramLoadError::FileSystem)?;
        identity
            .require(metadata, 1)
            .map_err(ProgramLoadError::FileSystem)?;
        let executable_source = source(inode)?;
        if let Some(header) = parse_script_header(executable_source.as_ref())? {
            if rewrite_count == MAX_SCRIPT_REWRITES {
                return Err(ProgramLoadError::InterpreterLoop);
            }
            let rewritten = rewrite_arguments(current_path, arguments, argument_bytes, header)?;
            current_path = rewritten.path;
            arguments = rewritten.arguments;
            argument_bytes = rewritten.argument_bytes;
            continue;
        }
        let (main, interpreter_path) = parse_main_elf(executable_source).map_err(parse_error)?;
        let interpreter = interpreter_path
            .map(|path| {
                let inode = vfs()
                    .open_at(Some(working_directory.clone()), &path, identity)
                    .map_err(ProgramLoadError::FileSystem)?;
                identity
                    .require(inode.metadata().map_err(ProgramLoadError::FileSystem)?, 1)
                    .map_err(ProgramLoadError::FileSystem)?;
                parse_interpreter_elf(source(inode)?).map_err(parse_error)
            })
            .transpose()?;
        return Ok(LoadedExecutable {
            image: ExecutableImage::new(main, interpreter),
            arguments,
            execfn,
            credentials: metadata,
        });
    }
    unreachable!("script rewrite loop exits through success or explicit limit")
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
    let file = RegularFile::from_inode(inode).map_err(ProgramLoadError::FileSystem)?;
    Ok(Arc::new(InodeExecutableSource { file, length }))
}

fn parse_script_header(
    source: &dyn ExecutableSource,
) -> Result<Option<ScriptHeader>, ProgramLoadError> {
    const BINPRM_BUF_SIZE: usize = 256;
    let read_size = source.len().min(BINPRM_BUF_SIZE);
    let mut probe = [0u8; BINPRM_BUF_SIZE];
    source
        .read_exact_at(0, &mut probe[..read_size])
        .map_err(|_| ProgramLoadError::FileSystem(FileSystemError::IoError))?;
    if !probe.starts_with(b"#!") {
        return Ok(None);
    }

    let newline = probe[..read_size].iter().position(|byte| *byte == b'\n');
    let line_end = newline.unwrap_or(read_size);
    let mut content = &probe[2..line_end];
    while content.last().is_some_and(|byte| is_space_tab(*byte)) {
        content = &content[..content.len() - 1];
    }
    let name_start = content
        .iter()
        .position(|byte| !is_space_tab(*byte))
        .ok_or(ProgramLoadError::InvalidExecutable)?;
    content = &content[name_start..];
    let name_end = content
        .iter()
        .position(|byte| is_space_tab(*byte) || *byte == 0)
        .unwrap_or(content.len());
    if name_end == 0
        || newline.is_none() && read_size == BINPRM_BUF_SIZE && name_end == content.len()
    {
        return Err(ProgramLoadError::InvalidExecutable);
    }
    let interpreter = copy_bytes(&content[..name_end])?;
    let argument = if content.get(name_end) == Some(&0) {
        None
    } else {
        let remainder = &content[name_end..];
        let remainder = &remainder[..remainder
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(remainder.len())];
        let argument_start = remainder.iter().position(|byte| !is_space_tab(*byte));
        argument_start
            .map(|start| copy_bytes(&remainder[start..]))
            .transpose()?
            .filter(|value| !value.is_empty())
    };
    Ok(Some(ScriptHeader {
        interpreter,
        argument,
    }))
}

fn rewrite_arguments(
    script_path: Vec<u8>,
    mut arguments: Vec<Vec<u8>>,
    argument_bytes: usize,
    header: ScriptHeader,
) -> Result<ScriptRewrite, ProgramLoadError> {
    let old_argv0 = arguments.remove(0);
    let interpreter_argv = copy_bytes(&header.interpreter)?;
    let mut next_bytes = argument_bytes
        .checked_sub(argument_cost(&old_argv0)?)
        .ok_or(ProgramLoadError::InvalidExecutable)?;
    for argument in [
        Some(interpreter_argv.as_slice()),
        header.argument.as_deref(),
        Some(script_path.as_slice()),
    ]
    .into_iter()
    .flatten()
    {
        next_bytes = next_bytes
            .checked_add(argument_cost(argument)?)
            .filter(|bytes| *bytes <= EXEC_ARGUMENT_BYTES_LIMIT)
            .ok_or(ProgramLoadError::ArgumentListTooLong)?;
    }

    let added = 2 + usize::from(header.argument.is_some());
    let mut rewritten = Vec::new();
    rewritten
        .try_reserve_exact(
            arguments
                .len()
                .checked_add(added)
                .ok_or(ProgramLoadError::ArgumentListTooLong)?,
        )
        .map_err(|_| ProgramLoadError::OutOfMemory)?;
    rewritten.push(interpreter_argv);
    if let Some(argument) = header.argument {
        rewritten.push(argument);
    }
    rewritten.push(script_path);
    rewritten.append(&mut arguments);
    Ok(ScriptRewrite {
        path: header.interpreter,
        arguments: rewritten,
        argument_bytes: next_bytes,
    })
}

fn normalize_arguments(
    arguments: &mut Vec<Vec<u8>>,
    argument_bytes: &mut usize,
) -> Result<(), ProgramLoadError> {
    if !arguments.is_empty() {
        return Ok(());
    }
    *argument_bytes = argument_bytes
        .checked_add(argument_cost(&[])?)
        .filter(|bytes| *bytes <= EXEC_ARGUMENT_BYTES_LIMIT)
        .ok_or(ProgramLoadError::ArgumentListTooLong)?;
    arguments
        .try_reserve_exact(1)
        .map_err(|_| ProgramLoadError::OutOfMemory)?;
    arguments.push(Vec::new());
    Ok(())
}

fn argument_cost(argument: &[u8]) -> Result<usize, ProgramLoadError> {
    core::mem::size_of::<usize>()
        .checked_add(argument.len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(ProgramLoadError::ArgumentListTooLong)
}

fn copy_bytes(source: &[u8]) -> Result<Vec<u8>, ProgramLoadError> {
    let mut destination = Vec::new();
    destination
        .try_reserve_exact(source.len())
        .map_err(|_| ProgramLoadError::OutOfMemory)?;
    destination.extend_from_slice(source);
    Ok(destination)
}

fn is_space_tab(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

fn parse_error(error: ExecutableParseError) -> ProgramLoadError {
    match error {
        ExecutableParseError::OutOfMemory => ProgramLoadError::OutOfMemory,
        ExecutableParseError::InvalidElf => ProgramLoadError::InvalidExecutable,
        ExecutableParseError::Io => ProgramLoadError::FileSystem(FileSystemError::IoError),
    }
}
