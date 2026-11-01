//! 静态 ELF64（`ET_EXEC`）装载：系统服务都是静态链接、固定地址的可执行文件。

use native_abi::Status;

use crate::{Vmar, Vmo};

const PT_LOAD: u32 = 1;
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
    let header_offset = u64_at(image, 32)?;
    let header_size = usize::from(u16_at(image, 54)?);
    let header_count = usize::from(u16_at(image, 56)?);
    if header_size < 56 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let page = native_abi::PAGE_SIZE;
    let base = vmar.region()?.base as usize;
    for index in 0..header_count {
        let header = header_offset
            .checked_add(index * header_size)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if u32_at(image, header)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(image, header + 4)?;
        let file_offset = u64_at(image, header + 8)?;
        let address = u64_at(image, header + 16)?;
        let file_size = u64_at(image, header + 32)?;
        let memory_size = u64_at(image, header + 40)?;
        if file_size > memory_size {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let start = address & !(page - 1);
        let end = address
            .checked_add(memory_size)
            .ok_or(native_abi::ERR_INVALID_ARGS)?
            .next_multiple_of(page);
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
    Ok(entry)
}
