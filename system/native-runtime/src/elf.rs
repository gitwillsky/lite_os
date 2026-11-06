//! 静态 ELF64（`ET_EXEC`）装载：系统服务都是静态链接、固定地址的可执行文件。

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

/// 把 `image` 的每个 `PT_LOAD` 复制进独立 VMO 并映射到 `vmar` 中的链接地址，返回入口地址。
///
/// 段之间不得共享页（链接脚本按页对齐各段）。
///
/// # Errors
///
/// 不是当前架构的静态可执行文件（`ERR_NOT_SUPPORTED`）、格式损坏或地址不在 `vmar` 内
/// （`ERR_INVALID_ARGS`），以及映射失败。
pub fn load(image: &[u8], vmar: &Vmar) -> Result<usize, Status> {
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
    let base = vmar.region()?.base as usize;
    let mut executable_entry = false;
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
        let offset = start
            .checked_sub(base)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        vmar.map(options, offset, &vmo, 0, end - start)?;
    }
    executable_entry
        .then_some(entry)
        .ok_or(native_abi::ERR_INVALID_ARGS)
}
