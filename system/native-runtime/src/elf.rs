//! 静态 ELF64（`ET_EXEC`）装载：系统服务都是静态链接、固定地址的可执行文件。

use alloc::vec::Vec;

use native_abi::Status;

use crate::{Vmar, Vmo};

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
#[cfg(target_arch = "aarch64")]
const EXPECTED_MACHINE: u16 = 183;
#[cfg(target_arch = "riscv64")]
const EXPECTED_MACHINE: u16 = 243;

fn field<const N: usize>(image: &[u8], offset: usize) -> Result<[u8; N], Status> {
    image
        .get(offset..offset.checked_add(N).ok_or(native_abi::ERR_INVALID_ARGS)?)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(native_abi::ERR_INVALID_ARGS)
}

fn u16_at(image: &[u8], offset: usize) -> Result<u16, Status> {
    field(image, offset).map(u16::from_le_bytes)
}

fn u32_at(image: &[u8], offset: usize) -> Result<u32, Status> {
    field(image, offset).map(u32::from_le_bytes)
}

fn u64_at(image: &[u8], offset: usize) -> Result<usize, Status> {
    field(image, offset).map(|bytes| u64::from_le_bytes(bytes) as usize)
}

fn programs(image: &[u8]) -> Result<core::slice::ChunksExact<'_, u8>, Status> {
    let offset = u64_at(image, 32)?;
    let size = usize::from(u16_at(image, 54)?);
    let count = usize::from(u16_at(image, 56)?);
    let invalid = native_abi::ERR_INVALID_ARGS;
    if size < 56 {
        return Err(invalid);
    }
    let end = offset.checked_add(count * size).ok_or(invalid)?;
    Ok(image.get(offset..end).ok_or(invalid)?.chunks_exact(size))
}

/// 检查 ELF 中是否有一个完全匹配 `expected` 的独立 PT_NOTE。
///
/// # Errors
///
/// 映像的 program header table 或 note 段范围损坏。
pub fn has_note(image: &[u8], expected: &[u8]) -> Result<bool, Status> {
    for header in programs(image)? {
        if u32_at(header, 0)? != PT_NOTE {
            continue;
        }
        let start = u64_at(header, 8)?;
        let size = u64_at(header, 32)?;
        let invalid = native_abi::ERR_INVALID_ARGS;
        let end = start.checked_add(size).ok_or(invalid)?;
        if image.get(start..end).ok_or(invalid)? == expected {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 已校验的静态 ELF 映像，持有只初始化一次的段内容模板。
///
/// 同一映像的启动者保留一个实例：各段逐进程写时复制，未修改的页共享。
/// 模板 VMO 不对外暴露、初始化后不再写入，故子进程的写入不会影响其他实例。
///
/// 来自 VMO 的映像（[`Image::from_vmo`]）零复制：段模板是源 VMO 页对齐区间的写时复制子对象，
/// 所有实例与 bootfs 共享同一份物理页，只有被写过的页才产生私有副本。
pub struct Image {
    entry: usize,
    parts: Vec<Part>,
}

/// 映射到进程地址空间的一段连续内容：文件部分（源 VMO 的子对象）或零填充的 bss。
struct Part {
    content: Vmo,
    start: usize,
    size: usize,
    options: u32,
}

/// 读取 `source` 中 `[offset, offset + length)`；越过 VMO 末尾是格式错误。
fn read_exact(source: &Vmo, offset: u64, length: usize) -> Result<Vec<u8>, Status> {
    let end = offset
        .checked_add(length as u64)
        .ok_or(native_abi::ERR_INVALID_ARGS)?;
    if end > source.size()? {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    buffer.resize(length, 0);
    source.read(&mut buffer, offset)?;
    Ok(buffer)
}

impl Image {
    /// 校验内存中的 `image` 并准备各 `PT_LOAD`；不保留输入字节的引用。
    ///
    /// 整个映像写入一个页对齐起点的 VMO（一次复制），各段再成为它的写时复制子对象。
    /// 已经在 VMO 里的映像（bootfs、交接给服务的映像）应使用 [`Self::from_vmo`] 避免这一次复制。
    ///
    /// # Errors
    ///
    /// 不是当前架构的静态可执行文件（`ERR_NOT_SUPPORTED`）、格式损坏
    /// （`ERR_INVALID_ARGS`），或分配/写入失败。失败时已准备的段随映像一起释放。
    pub fn parse(image: &[u8]) -> Result<Self, Status> {
        let page = native_abi::PAGE_SIZE;
        let source = Vmo::create(image.len().next_multiple_of(page).max(page) as u64, 0)?;
        source.write(image, 0)?;
        Self::from_vmo(&source, 0)
    }

    /// 校验 `source` 中起点为 `base` 的 ELF 并准备各 `PT_LOAD` 的段模板。
    ///
    /// 文件内容按页对齐的段（`p_offset ≡ p_vaddr` 且 `base` 页对齐，即链接脚本按页对齐各段）零复制：
    /// 模板是 `source` 的写时复制子对象，文件末页中越过 `p_filesz` 的字节在模板内清零，
    /// `p_memsz` 超出文件页的 bss 部分由独立的匿名零页 VMO 提供。不满足对齐的段回退为逐段复制。
    /// 段之间不得共享页。
    ///
    /// # Errors
    ///
    /// 同 [`Self::parse`]；段范围越过 `source` 末尾为 `ERR_INVALID_ARGS`。
    pub fn from_vmo(source: &Vmo, base: u64) -> Result<Self, Status> {
        let page = native_abi::PAGE_SIZE;
        let invalid = native_abi::ERR_INVALID_ARGS;
        let header = read_exact(source, base, 64)?;
        if header.get(..4) != Some(b"\x7fELF".as_slice())
            || header.get(4) != Some(&2)
            || header.get(5) != Some(&1)
        {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        if u16_at(&header, 16)? != ET_EXEC || u16_at(&header, 18)? != EXPECTED_MACHINE {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let entry = u64_at(&header, 24)?;
        let table_offset = u64_at(&header, 32)?;
        let entry_size = usize::from(u16_at(&header, 54)?);
        let count = usize::from(u16_at(&header, 56)?);
        if entry_size < 56
            || count
                .checked_mul(entry_size)
                .is_none_or(|bytes| bytes > 1 << 16)
        {
            return Err(invalid);
        }
        let table = read_exact(
            source,
            base.checked_add(table_offset as u64).ok_or(invalid)?,
            count * entry_size,
        )?;
        let mut executable_entry = false;
        let mut parts = Vec::new();
        for program in table.chunks_exact(entry_size) {
            if u32_at(program, 0)? != PT_LOAD {
                continue;
            }
            let flags = u32_at(program, 4)?;
            let file_offset = u64_at(program, 8)?;
            let address = u64_at(program, 16)?;
            let file_size = u64_at(program, 32)?;
            let memory_size = u64_at(program, 40)?;
            if file_size > memory_size {
                return Err(invalid);
            }
            if memory_size == 0 {
                continue;
            }
            let start = address & !(page - 1);
            let memory_end = address.checked_add(memory_size).ok_or(invalid)?;
            executable_entry |= flags & PF_X != 0 && (address..memory_end).contains(&entry);
            let end = memory_end.checked_next_multiple_of(page).ok_or(invalid)?;
            let mut options = native_abi::vm::SPECIFIC;
            if flags & PF_R != 0 {
                options |= native_abi::vm::READ;
            }
            if flags & PF_W != 0 {
                options |= native_abi::vm::WRITE;
            }
            if flags & PF_X != 0 {
                options |= native_abi::vm::EXECUTE;
            }
            let file_end = base
                .checked_add(file_offset as u64)
                .and_then(|offset| offset.checked_add(file_size as u64))
                .ok_or(invalid)?;
            if file_end > source.size()? {
                return Err(invalid);
            }
            let head = address - start;
            let aligned = base.is_multiple_of(page as u64) && file_offset % page == head;
            parts
                .try_reserve(2)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            // 文件部分覆盖的字节数（页对齐）；bss 页在其后。
            let file_bytes = if file_size == 0 {
                0
            } else {
                (head + file_size).next_multiple_of(page)
            };
            if file_bytes != 0 {
                let content = if aligned {
                    let child = source.create_child(
                        native_abi::vmo::CHILD_SNAPSHOT_AT_LEAST_ON_WRITE,
                        base + (file_offset - head) as u64,
                        file_bytes as u64,
                    )?;
                    // 末页里越过 p_filesz 的字节属于文件的下一段内容；模板内清零（私有副本，不影响源）。
                    let tail = head + file_size;
                    if tail < file_bytes {
                        child.op_range(
                            native_abi::vmo::OP_ZERO,
                            tail as u64,
                            (file_bytes - tail) as u64,
                        )?;
                    }
                    child
                } else {
                    let vmo = Vmo::create(file_bytes as u64, 0)?;
                    let bytes = read_exact(source, base + file_offset as u64, file_size)?;
                    vmo.write(&bytes, head as u64)?;
                    vmo
                };
                parts.push(Part {
                    content,
                    start,
                    size: file_bytes,
                    options,
                });
            }
            if end - start > file_bytes {
                parts.push(Part {
                    content: Vmo::create((end - start - file_bytes) as u64, 0)?,
                    start: start + file_bytes,
                    size: end - start - file_bytes,
                    options,
                });
            }
        }
        executable_entry
            .then_some(Self { entry, parts })
            .ok_or(invalid)
    }

    /// 把映像映射到 `vmar` 的链接地址，返回入口地址。
    ///
    /// 映射保活段内容，因而启动者可在最后一次启动后释放本映像。
    ///
    /// # Errors
    ///
    /// 地址不在 VMAR 内、目标范围已被占用，或创建 COW 子对象/映射失败。
    /// 已完成的映射由调用者的进程清理路径回收。
    pub(crate) fn load(&self, vmar: &Vmar) -> Result<usize, Status> {
        let base = vmar.region()?.base as usize;
        for segment in &self.parts {
            // 1. 模板永不再写入，单向 COW 即可共享内容，无需建立 hidden-parent。
            // 2. 只读段也使用私有视图：之后提升映射写权限或调试写入时，不能改动其他实例。
            let content = segment.content.create_child(
                native_abi::vmo::CHILD_SNAPSHOT_AT_LEAST_ON_WRITE,
                0,
                segment.size as u64,
            )?;
            let offset = segment
                .start
                .checked_sub(base)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            vmar.map(segment.options, offset, &content, 0, segment.size)?;
        }
        Ok(self.entry)
    }
}
