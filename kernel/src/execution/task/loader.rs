use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{AccessIdentity, FileSystemError, InodeMetadata, InodeType, OpenedFile, RegularFile, vfs},
    memory::{
        ElfLoadError, ExecutableImage, ExecutableParseError, ExecutableSource, InitialStack,
        InitialStackCredentials, MemorySet, parse_interpreter_elf, parse_main_elf,
    },
};

/// argv/envp strings、NUL 与 pointer slots 共用的 exec byte budget。
pub(crate) const EXEC_ARGUMENT_BYTES_LIMIT: usize = 128 * 1024;

/// pathname、script rewrite、权限检查与 ELF mapping plan 的完整加载结果。
pub(crate) struct LoadedExecutable {
    image: ExecutableImage,
    arguments: Vec<Vec<u8>>,
    execfn: Vec<u8>,
    credentials: InodeMetadata,
    file_capability: Option<syscall_abi::file_capability::FileCapabilityData>,
    readable: bool,
    // interpreter deny-write 持续到 image 构造完成；其 VMA 不延长这份加载期租约。
    _interpreter_access: Option<Arc<ExecutableOpenLease>>,
}

impl LoadedExecutable {
    /// 从最终 ELF plan 与 rewritten argv transactionally 构造新地址空间。
    ///
    /// # Parameters
    ///
    /// - `environments`: 已从 userspace 完整复制且不含 NUL 的 envp strings。
    ///
    /// # Returns
    ///
    /// 新 MemorySet、initial sp 与 entry point。
    ///
    /// # Errors
    ///
    /// ELF mapping、initial stack、source I/O 或资源失败。
    pub(super) fn build_address_space(
        &self,
        environments: &[Vec<u8>],
        credentials: InitialStackCredentials,
        stack_limit: u64,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<(MemorySet, usize, usize), ElfLoadError> {
        // 1. exec owns source I/O；未发布地址空间前完成 entropy，失败不替换旧 image。
        let mut entropy =
            crate::random::EntropyBatch::<16>::try_new().ok_or(ElfLoadError::OutOfMemory)?;
        let random = entropy.fill(16).map_err(|_| ElfLoadError::Io)?;
        // fill 成功返回恰好请求的 16 bytes；固定数组让 memory 无需重验 AT_RANDOM 长度。
        let random = random.try_into().expect("AT_RANDOM entropy length");
        // 2. memory 只消费 immutable 栈输入，不调用设备或 entropy facade。
        MemorySet::from_elf(
            &self.image,
            InitialStack {
                credentials,
                arguments: &self.arguments,
                environments,
                execfn: &self.execfn,
                random,
            },
            stack_limit,
            address_space_limit,
            data_limit,
        )
    }

    /// 返回用户传给 execve 的原始 pathname，用于 AT_EXECFN 与 process comm。
    ///
    /// # Returns
    ///
    /// 不含 NUL 的 immutable pathname bytes。
    pub(super) fn execfn(&self) -> &[u8] {
        &self.execfn
    }

    /// main ELF 与实际 PT_INTERP 的 MAY_READ 结果，供 exec mm dumpability使用。
    pub(super) fn readable(&self) -> bool {
        self.readable
    }

    pub(super) fn credential_metadata(&self) -> InodeMetadata {
        self.credentials
    }

    /// 最终main ELF实际path上的immutable file-cap快照；script与PT_INTERP不提供exec凭据。
    pub(super) fn file_capability(
        &self,
    ) -> Option<syscall_abi::file_capability::FileCapabilityData> {
        self.file_capability
    }
}

/// executable pathname 解析、权限检查与 mapping plan 构造失败原因。
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

/// Actual executable-open lease；与随机读取 source 分离，只有 main mm 长期持有 deny-write。
pub(super) struct ExecutableOpenLease {
    _denial: crate::fs::InodeAccessLease,
    _opened: Arc<crate::fs::OpenedFile>,
}
impl ExecutableOpenLease {
    pub(super) fn opened(&self) -> Arc<OpenedFile> {
        self._opened.clone()
    }
}
impl core::fmt::Debug for ExecutableOpenLease {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ExecutableOpenLease")
            .finish_non_exhaustive()
    }
}
impl crate::memory::MappedFileOwner for ExecutableOpenLease {}

struct InodeExecutableSource {
    // Storage inode/cache pins retire before the actual path releases its filesystem/view lease.
    file: RegularFile,
    length: usize,
    _opened: Arc<crate::fs::OpenedFile>,
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

/// 解析 pathname，按 Linux binfmt_script 规则重写 argv，最终构造唯一 ELF image。
///
/// # Parameters
///
/// - `origin`: relative pathname 与 relative script interpreter 的解析起点。
/// - `path`: 用户传入且不含 NUL 的原始 exec pathname；成功后成为 AT_EXECFN。
/// - `arguments`: 已从 userspace 完整复制的 argv。
/// - `argument_bytes`: 当前 argv/envp 的受限 byte accounting。
///
/// # Returns
///
/// 最终 ELF image、重写后的 argv 与原始 AT_EXECFN pathname。
///
/// # Errors
///
/// 返回 pathname、权限、资源、argument limit、interpreter loop 或格式错误。
pub(crate) fn load_executable(
    origin: crate::fs::LookupOrigin,
    namespace: &Arc<crate::fs::MountNamespace>,
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
        let executable = vfs()
            .open_file_at(origin.clone(), &current_path, identity)
            .map_err(ProgramLoadError::FileSystem)?;
        let inode = executable.inode();
        // may_open 的初次 DAC 检查先于 deny-write；身份授权 tuple 只能在实际 execution lease 后捕获。
        identity
            .require(inode.metadata().map_err(ProgramLoadError::FileSystem)?, 1)
            .map_err(ProgramLoadError::FileSystem)?;
        let (executable_source, execution_owner) = source(&executable)?;
        // Writer 在初次检查后仍可能清除 set-ID 并关闭；此处重取同一 canonical owner 的 live tuple。
        let mut metadata = inode.metadata().map_err(ProgramLoadError::FileSystem)?;
        identity
            .require(metadata, 1)
            .map_err(ProgramLoadError::FileSystem)?;
        if !executable.may_suid(namespace, &identity.user_namespace()) {
            metadata.mode &= !0o6000;
        }
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
        let file_capability = executable
            .executable_capability(namespace, &identity.user_namespace())
            .map_err(ProgramLoadError::FileSystem)?;
        let mut readable = identity.permits(metadata, 4);
        let mut interpreter_access = None;
        let interpreter = interpreter_path
            .map(|path| {
                let opened = vfs()
                    .open_file_at(origin.clone(), &path, identity)
                    .map_err(ProgramLoadError::FileSystem)?;
                let inode = opened.inode();
                let interpreter_metadata =
                    inode.metadata().map_err(ProgramLoadError::FileSystem)?;
                identity
                    .require(interpreter_metadata, 1)
                    .map_err(ProgramLoadError::FileSystem)?;
                let (source, access) = source(&opened)?;
                let interpreter_metadata =
                    inode.metadata().map_err(ProgramLoadError::FileSystem)?;
                identity
                    .require(interpreter_metadata, 1)
                    .map_err(ProgramLoadError::FileSystem)?;
                readable &= identity.permits(interpreter_metadata, 4);
                let interpreter = parse_interpreter_elf(source).map_err(parse_error)?;
                interpreter_access = Some(access);
                Ok(interpreter)
            })
            .transpose()?;
        return Ok(LoadedExecutable {
            image: ExecutableImage::new(main, interpreter, execution_owner),
            _interpreter_access: interpreter_access,
            arguments,
            execfn,
            credentials: metadata,
            file_capability,
            readable,
        });
    }
    unreachable!("script rewrite loop exits through success or explicit limit")
}

fn source(
    opened: &Arc<crate::fs::OpenedFile>,
) -> Result<(Arc<dyn ExecutableSource>, Arc<ExecutableOpenLease>), ProgramLoadError> {
    let inode = opened.inode();
    if inode.inode_type() != InodeType::File {
        return Err(ProgramLoadError::NotRegularFile);
    }
    if !inode.is_executable() {
        return Err(ProgramLoadError::NotExecutable);
    }
    // Linux `may_open` 对 exec 的检查：noexec 挂载上的文件（含解释器）一律拒绝。
    if opened.mount_flags().noexec() {
        return Err(ProgramLoadError::FileSystem(FileSystemError::AccessDenied));
    }
    let denial = crate::fs::InodeAccessLease::execute(inode.clone())
        .map_err(ProgramLoadError::FileSystem)?;
    let owner = Arc::try_new(ExecutableOpenLease {
        _denial: denial,
        _opened: opened.clone(),
    })
    .map_err(|_| ProgramLoadError::OutOfMemory)?;
    opened.touch_access();
    let length = usize::try_from(inode.size())
        .map_err(|_| ProgramLoadError::FileSystem(FileSystemError::IoError))?;
    let file = RegularFile::from_inode(inode).map_err(ProgramLoadError::FileSystem)?;
    let source = Arc::try_new(InodeExecutableSource {
        file,
        length,
        _opened: opened.clone(),
    })
    .map_err(|_| ProgramLoadError::OutOfMemory)?;
    Ok((source, owner))
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

pub(super) fn argument_cost(argument: &[u8]) -> Result<usize, ProgramLoadError> {
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
