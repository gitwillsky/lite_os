use crate::memory::{address::VirtualAddress, config, executable::ParsedElf, page_table::PTEFlags};

use super::{
    ElfLoadError, LoadedElf, MapArea, MapPermission, MemorySet, PageFaultAccess, PageFaultOutcome,
    PrivateFileArea,
};

impl MemorySet {
    /// @description 按唯一的已解析映射计划装载 ELF；segment bytes 逐页来自 source。
    ///
    /// @param image 单次 parser 产生的 immutable ELF mapping plan。
    /// @param load_bias ET_EXEC 为零，PIE 或 interpreter 使用固定非零基址。
    /// @return entry、auxv program-header facts 与最高 segment end。
    /// @errors 地址、权限、映射、资源或 source 读取失败；调用方丢弃新 MemorySet。
    pub(super) fn map_elf_image(
        &mut self,
        image: &ParsedElf,
        load_bias: usize,
    ) -> Result<LoadedElf, ElfLoadError> {
        let ph_end = image
            .program_header_offset
            .checked_add(
                image
                    .program_header_entry_size
                    .checked_mul(image.program_header_count)
                    .ok_or(ElfLoadError::InvalidElf)?,
            )
            .ok_or(ElfLoadError::InvalidElf)?;
        let mut max_end = 0usize;
        let mut phdr = None;
        for segment in &image.load_segments {
            let start = load_bias
                .checked_add(segment.virtual_address)
                .ok_or(ElfLoadError::InvalidElf)?;
            let end = start
                .checked_add(segment.memory_size)
                .ok_or(ElfLoadError::InvalidElf)?;
            let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
            if start == 0 || start >= end || end > user_end {
                return Err(ElfLoadError::InvalidElf);
            }
            let mut permission = MapPermission::U;
            if segment.flags & 4 != 0 {
                permission |= MapPermission::R;
            }
            if segment.flags & 2 != 0 {
                permission |= MapPermission::W;
            }
            if segment.flags & 1 != 0 {
                permission |= MapPermission::X;
            }
            let backing = PrivateFileArea::executable(
                image.source.clone(),
                start,
                segment.file_offset,
                segment.file_size,
            );
            self.push(
                MapArea::elf(start.into(), end.into(), permission, backing),
                None,
            )
            .map_err(ElfLoadError::from)?;
            max_end = max_end.max(end);
            let file_end = segment
                .file_offset
                .checked_add(segment.file_size)
                .ok_or(ElfLoadError::InvalidElf)?;
            if segment.file_offset <= image.program_header_offset && ph_end <= file_end {
                phdr = start.checked_add(image.program_header_offset - segment.file_offset);
            }
        }
        let entry = load_bias
            .checked_add(image.entry)
            .ok_or(ElfLoadError::InvalidElf)?;
        if self.handle_page_fault(entry, PageFaultAccess::Execute)? != PageFaultOutcome::Handled {
            return Err(ElfLoadError::InvalidElf);
        }
        let entry_pte = self
            .translate(VirtualAddress::from(entry).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !entry_pte.flags().contains(PTEFlags::U | PTEFlags::X) {
            return Err(ElfLoadError::InvalidElf);
        }
        Ok(LoadedElf {
            entry,
            phdr,
            phent: image.program_header_entry_size,
            phnum: image.program_header_count,
            max_end,
        })
    }
}
