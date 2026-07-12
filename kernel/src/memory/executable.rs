use alloc::{sync::Arc, vec::Vec};

/// @description 可执行文件的只读随机访问 seam；文件系统 adapter 是唯一具体实现 owner。
pub(crate) trait ExecutableSource: Send + Sync {
    /// @description 返回创建 source 时观察到的文件长度。
    ///
    /// @return 后续 read boundary validation 使用的稳定 byte length。
    fn len(&self) -> usize;

    /// @description 从指定文件偏移完整读取目标缓冲区，short read 与 I/O error 均失败。
    ///
    /// @param offset 文件起始位置的 byte offset。
    /// @param buffer 必须完整填充的 destination slice。
    /// @return 完整读取返回 unit。
    /// @errors source I/O error、越界或 short read 返回错误。
    fn read_exact_at(&self, offset: usize, buffer: &mut [u8]) -> Result<(), ()>;
}

/// @description ELF object type；只保留当前 loader 接受的 ET_EXEC 与 ET_DYN。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ElfKind {
    Executable,
    SharedObject,
}

/// @description 已校验的 PT_LOAD 描述；不复制 segment 文件内容。
pub(super) struct LoadSegment {
    pub(super) file_offset: usize,
    pub(super) file_size: usize,
    pub(super) virtual_address: usize,
    pub(super) memory_size: usize,
    pub(super) flags: u32,
}

/// @description 单次解析得到的 ELF 映射计划；source 是 segment bytes 的唯一来源。
pub(crate) struct ParsedElf {
    pub(super) source: Arc<dyn ExecutableSource>,
    pub(super) kind: ElfKind,
    pub(super) entry: usize,
    pub(super) program_header_offset: usize,
    pub(super) program_header_entry_size: usize,
    pub(super) program_header_count: usize,
    pub(super) load_segments: Vec<LoadSegment>,
}

/// @description exec transaction 使用的主程序与可选动态解释器映射计划。
pub(crate) struct ExecutableImage {
    pub(super) main: ParsedElf,
    pub(super) interpreter: Option<ParsedElf>,
}

impl ExecutableImage {
    /// @description 组合由同一 ELF parser 产生的主程序与动态解释器映射计划。
    ///
    /// @param main 已校验的主程序映射计划。
    /// @param interpreter PT_INTERP 指向且已独立校验的动态解释器映射计划。
    /// @return 单次 exec transaction 的完整 immutable input。
    pub(crate) fn new(main: ParsedElf, interpreter: Option<ParsedElf>) -> Self {
        Self { main, interpreter }
    }
}

/// @description bounded ELF parsing 的稳定失败分类，不泄漏 parser 实现细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutableParseError {
    /// metadata buffer 或 owned mapping plan 分配失败。
    OutOfMemory,
    /// ELF header、program header 或 PT_INTERP 不满足 RV64 loader contract。
    InvalidElf,
    /// executable source 发生 I/O error、越界或 short read。
    Io,
}

/// @description 解析主程序并提取唯一 PT_INTERP pathname。
///
/// @param source 可执行文件的只读随机访问源。
/// @return 已校验映射计划与可选的绝对 PT_INTERP path。
/// @errors 返回资源耗尽、非法 ELF 或 source 读取失败。
pub(crate) fn parse_main_elf(
    source: Arc<dyn ExecutableSource>,
) -> Result<(ParsedElf, Option<Vec<u8>>), ExecutableParseError> {
    parse_elf(source, true)
}

/// @description 解析动态解释器；出现嵌套 PT_INTERP 时直接拒绝。
///
/// @param source 动态解释器的只读随机访问源。
/// @return 已校验的唯一映射计划。
/// @errors 返回资源耗尽、非法 ELF 或 source 读取失败。
pub(crate) fn parse_interpreter_elf(
    source: Arc<dyn ExecutableSource>,
) -> Result<ParsedElf, ExecutableParseError> {
    let (image, interpreter) = parse_elf(source, false)?;
    debug_assert!(interpreter.is_none());
    Ok(image)
}

fn parse_elf(
    source: Arc<dyn ExecutableSource>,
    allow_interpreter: bool,
) -> Result<(ParsedElf, Option<Vec<u8>>), ExecutableParseError> {
    const HEADER_SIZE: usize = 64;
    const PH_SIZE: usize = 56;
    const MAX_PROGRAM_HEADERS_SIZE: usize = 64 * 1024;
    const MAX_INTERPRETER_PATH: usize = 4096;

    let mut header = [0u8; HEADER_SIZE];
    if source.len() < HEADER_SIZE {
        return Err(ExecutableParseError::InvalidElf);
    }
    source
        .read_exact_at(0, &mut header)
        .map_err(|_| ExecutableParseError::Io)?;
    if &header[..7] != b"\x7fELF\x02\x01\x01"
        || read_u16(&header, 18)? != 243
        || read_u32(&header, 20)? != 1
        || read_u16(&header, 52)? as usize != HEADER_SIZE
    {
        return Err(ExecutableParseError::InvalidElf);
    }
    let kind = match read_u16(&header, 16)? {
        2 => ElfKind::Executable,
        3 => ElfKind::SharedObject,
        _ => return Err(ExecutableParseError::InvalidElf),
    };
    let flags = read_u32(&header, 48)?;
    if flags & !0x7 != 0 || flags & 0x6 == 0x6 {
        return Err(ExecutableParseError::InvalidElf);
    }
    let entry = usize_u64(&header, 24)?;
    let program_header_offset = usize_u64(&header, 32)?;
    let program_header_entry_size = read_u16(&header, 54)? as usize;
    let program_header_count = read_u16(&header, 56)? as usize;
    let table_size = program_header_entry_size
        .checked_mul(program_header_count)
        .filter(|size| {
            program_header_count != 0
                && program_header_entry_size == PH_SIZE
                && *size <= MAX_PROGRAM_HEADERS_SIZE
        })
        .ok_or(ExecutableParseError::InvalidElf)?;
    program_header_offset
        .checked_add(table_size)
        .filter(|end| program_header_offset >= HEADER_SIZE && *end <= source.len())
        .ok_or(ExecutableParseError::InvalidElf)?;

    let mut table = zeroed(table_size)?;
    source
        .read_exact_at(program_header_offset, &mut table)
        .map_err(|_| ExecutableParseError::Io)?;
    let mut load_segments = Vec::new();
    load_segments
        .try_reserve(program_header_count)
        .map_err(|_| ExecutableParseError::OutOfMemory)?;
    let mut interpreter = None;
    for ph in table.as_chunks::<PH_SIZE>().0 {
        let ph_type = read_u32(ph, 0)?;
        let ph_flags = read_u32(ph, 4)?;
        let file_offset = usize_u64(ph, 8)?;
        let virtual_address = usize_u64(ph, 16)?;
        let file_size = usize_u64(ph, 32)?;
        let memory_size = usize_u64(ph, 40)?;
        let alignment = usize_u64(ph, 48)?;
        file_offset
            .checked_add(file_size)
            .filter(|end| *end <= source.len())
            .ok_or(ExecutableParseError::InvalidElf)?;
        match ph_type {
            1 => {
                if file_size > memory_size
                    || alignment > 1
                        && (!alignment.is_power_of_two()
                            || virtual_address % alignment != file_offset % alignment)
                    || ph_flags & 0x3 == 0x3
                {
                    return Err(ExecutableParseError::InvalidElf);
                }
                if memory_size == 0 {
                    if file_size != 0 {
                        return Err(ExecutableParseError::InvalidElf);
                    }
                    continue;
                }
                load_segments.push(LoadSegment {
                    file_offset,
                    file_size,
                    virtual_address,
                    memory_size,
                    flags: ph_flags,
                });
            }
            3 if allow_interpreter => {
                if interpreter.is_some() || !(2..=MAX_INTERPRETER_PATH).contains(&file_size) {
                    return Err(ExecutableParseError::InvalidElf);
                }
                let mut path = zeroed(file_size)?;
                source
                    .read_exact_at(file_offset, &mut path)
                    .map_err(|_| ExecutableParseError::Io)?;
                path.pop()
                    .filter(|byte| *byte == 0)
                    .ok_or(ExecutableParseError::InvalidElf)?;
                if path.first() != Some(&b'/') || path.contains(&0) {
                    return Err(ExecutableParseError::InvalidElf);
                }
                interpreter = Some(path);
            }
            3 => return Err(ExecutableParseError::InvalidElf),
            2 | 7 => {}
            0x6474_e551 if ph_flags & 1 != 0 => {
                return Err(ExecutableParseError::InvalidElf);
            }
            _ => {}
        }
    }
    if load_segments.is_empty() {
        return Err(ExecutableParseError::InvalidElf);
    }
    Ok((
        ParsedElf {
            source,
            kind,
            entry,
            program_header_offset,
            program_header_entry_size,
            program_header_count,
            load_segments,
        },
        interpreter,
    ))
}

fn zeroed(size: usize) -> Result<Vec<u8>, ExecutableParseError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| ExecutableParseError::OutOfMemory)?;
    bytes.resize(size, 0);
    Ok(bytes)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ExecutableParseError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(ExecutableParseError::InvalidElf)?
            .try_into()
            .unwrap(),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ExecutableParseError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(ExecutableParseError::InvalidElf)?
            .try_into()
            .unwrap(),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ExecutableParseError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(ExecutableParseError::InvalidElf)?
            .try_into()
            .unwrap(),
    ))
}

fn usize_u64(bytes: &[u8], offset: usize) -> Result<usize, ExecutableParseError> {
    usize::try_from(read_u64(bytes, offset)?).map_err(|_| ExecutableParseError::InvalidElf)
}
