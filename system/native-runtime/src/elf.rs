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

/// 已校验的静态 ELF 映像，持有只初始化一次的段内容。
///
/// 同一映像的启动者保留一个实例：各段逐进程写时复制，未修改的页共享。
/// 模板 VMO 不对外暴露、初始化后不再写入，故子进程的写入不会影响其他实例。
pub struct Image {
    entry: usize,
    segments: Vec<Segment>,
}

struct Segment {
    content: Vmo,
    start: usize,
    size: usize,
    options: u32,
}

impl Image {
    /// 校验 `image` 并准备各 `PT_LOAD` 的页对齐内容；不保留输入字节的引用。
    ///
    /// 段之间不得共享页（链接脚本按页对齐各段）。
    ///
    /// # Errors
    ///
    /// 不是当前架构的静态可执行文件（`ERR_NOT_SUPPORTED`）、格式损坏
    /// （`ERR_INVALID_ARGS`），或分配/写入失败。失败时已准备的段随映像一起释放。
    pub fn parse(image: &[u8]) -> Result<Self, Status> {
        if image.get(..4) != Some(b"\x7fELF".as_slice())
            || image.get(4) != Some(&2)
            || image.get(5) != Some(&1)
        {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        if u16_at(image, 16)? != ET_EXEC || u16_at(image, 18)? != EXPECTED_MACHINE {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        let entry = u64_at(image, 24)?;
        let page = native_abi::PAGE_SIZE;
        let mut executable_entry = false;
        let mut segments = Vec::new();
        for header in programs(image)? {
            if u32_at(header, 0)? != PT_LOAD {
                continue;
            }
            let flags = u32_at(header, 4)?;
            let file_offset = u64_at(header, 8)?;
            let address = u64_at(header, 16)?;
            let file_size = u64_at(header, 32)?;
            let memory_size = u64_at(header, 40)?;
            if file_size > memory_size {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let start = address & !(page - 1);
            let memory_end = address
                .checked_add(memory_size)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            if memory_size == 0 {
                continue;
            }
            executable_entry |= flags & PF_X != 0 && (address..memory_end).contains(&entry);
            let end = memory_end
                .checked_next_multiple_of(page)
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            let content = image
                .get(
                    file_offset
                        ..file_offset
                            .checked_add(file_size)
                            .ok_or(native_abi::ERR_INVALID_ARGS)?,
                )
                .ok_or(native_abi::ERR_INVALID_ARGS)?;
            let vmo = Vmo::create((end - start) as u64, 0)?;
            vmo.write(content, (address - start) as u64)?;
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
            segments
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            segments.push(Segment {
                content: vmo,
                start,
                size: end - start,
                options,
            });
        }
        executable_entry
            .then_some(Self { entry, segments })
            .ok_or(native_abi::ERR_INVALID_ARGS)
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
        for segment in &self.segments {
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
