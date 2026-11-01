//! 根服务的静态 ELF64 装载：只接受当前架构的 `ET_EXEC`，把每个 `PT_LOAD` 复制进独立 VMO
//! 并映射到指定地址。其余进程由用户态自行装载。

use alloc::sync::Arc;

use crate::vm::page_table::PagePermissions;
use crate::vm::{Placement, Vmar, Vmo, VmError, VmoOptions};

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const ET_EXEC: u16 = 2;
#[cfg(target_arch = "aarch64")]
const EXPECTED_MACHINE: u16 = 183;
#[cfg(target_arch = "riscv64")]
const EXPECTED_MACHINE: u16 = 243;

/// 装载失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ElfError {
    /// 头部或程序头越界、字段不一致。
    Malformed,
    /// 不是当前架构的 64 位静态可执行文件。
    Unsupported,
    /// 映射或复制失败。
    Memory(VmError),
}

impl From<VmError> for ElfError {
    fn from(error: VmError) -> Self {
        Self::Memory(error)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let value = bytes.get(offset..offset + 2).ok_or(ElfError::Malformed)?;
    Ok(u16::from_le_bytes(value.try_into().expect("two bytes")))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let value = bytes.get(offset..offset + 4).ok_or(ElfError::Malformed)?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let value = bytes.get(offset..offset + 8).ok_or(ElfError::Malformed)?;
    Ok(u64::from_le_bytes(value.try_into().expect("eight bytes")))
}

/// 装载 `image` 到 `vmar`，返回入口地址。
///
/// # Errors
/// 见 [`ElfError`]。
pub(super) fn load(image: &[u8], vmar: &Arc<Vmar>) -> Result<usize, ElfError> {
    if image.get(..4) != Some(b"\x7fELF".as_slice()) || image.get(4) != Some(&2) || image.get(5) != Some(&1) {
        return Err(ElfError::Unsupported);
    }
    if read_u16(image, 16)? != ET_EXEC || read_u16(image, 18)? != EXPECTED_MACHINE {
        return Err(ElfError::Unsupported);
    }
    let entry = read_u64(image, 24)? as usize;
    let header_offset = read_u64(image, 32)? as usize;
    let header_size = usize::from(read_u16(image, 54)?);
    let header_count = usize::from(read_u16(image, 56)?);
    if header_size < 56 {
        return Err(ElfError::Malformed);
    }
    let page = native_abi::PAGE_SIZE;
    let base = vmar.info().base;
    for index in 0..header_count {
        let header = header_offset
            .checked_add(index * header_size)
            .ok_or(ElfError::Malformed)?;
        if read_u32(image, header)? != PT_LOAD {
            continue;
        }
        let flags = read_u32(image, header + 4)?;
        let file_offset = read_u64(image, header + 8)? as usize;
        let address = read_u64(image, header + 16)? as usize;
        let file_size = read_u64(image, header + 32)? as usize;
        let memory_size = read_u64(image, header + 40)? as usize;
        if file_size > memory_size {
            return Err(ElfError::Malformed);
        }
        let start = address & !(page - 1);
        let end = address
            .checked_add(memory_size)
            .ok_or(ElfError::Malformed)?
            .next_multiple_of(page);
        let content = image
            .get(file_offset..file_offset.checked_add(file_size).ok_or(ElfError::Malformed)?)
            .ok_or(ElfError::Malformed)?;
        let vmo = Vmo::create((end - start) as u64, VmoOptions::default())?;
        vmo.write((address - start) as u64, content)?;
        let mut permissions = PagePermissions::empty();
        if flags & PF_R != 0 {
            permissions |= PagePermissions::READ;
        }
        if flags & PF_W != 0 {
            permissions |= PagePermissions::WRITE;
        }
        if flags & PF_X != 0 {
            permissions |= PagePermissions::EXECUTE;
        }
        let offset = start.checked_sub(base).ok_or(ElfError::Malformed)?;
        vmar.map(
            Placement::Specific(offset),
            &vmo,
            0,
            end - start,
            permissions,
            PagePermissions::READ | PagePermissions::WRITE | PagePermissions::EXECUTE,
        )?;
    }
    Ok(entry)
}
