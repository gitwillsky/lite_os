use alloc::sync::Arc;

use crate::memory::{
    address::VirtualAddress,
    config,
    executable::{ExecutableSource, ParsedElf},
    page_table::PTEFlags,
};

use super::{ElfLoadError, LoadedElf, MapArea, MapPermission, MemorySet};

impl MapArea {
    /// @description 将一个 PT_LOAD 的文件区间直接填入新分配页，不创建整文件副本。
    ///
    /// @param source segment bytes 的唯一随机读来源。
    /// @param offset PT_LOAD 在 source 中的起始 byte offset。
    /// @param size 需要复制的 p_filesz；剩余 p_memsz 保持零填充。
    /// @return 所有 source bytes 恰好写入 frame 后返回 unit。
    /// @errors source 读取失败返回 Io，映射容量不足返回 InvalidElf。
    fn copy_from_source(
        &mut self,
        source: &dyn ExecutableSource,
        offset: usize,
        size: usize,
    ) -> Result<(), ElfLoadError> {
        let mut copied = 0usize;
        for (index, frame) in self.data_frames.values_mut().enumerate() {
            if copied == size {
                break;
            }
            let page_offset = if index == 0 { self.data_page_offset } else { 0 };
            let count = (config::PAGE_SIZE - page_offset).min(size - copied);
            let destination = &mut Arc::get_mut(frame)
                .expect("new executable frame must be uniquely owned")
                .bytes_mut()[page_offset..page_offset + count];
            source
                .read_exact_at(offset + copied, destination)
                .map_err(|_| ElfLoadError::Io)?;
            copied += count;
        }
        (copied == size)
            .then_some(())
            .ok_or(ElfLoadError::InvalidElf)
    }
}

impl MemorySet {
    /// @description transactionally 映射并填充一个 executable VMA，读取失败时回滚全部新页。
    ///
    /// @param area 尚未发布到 VMA owner 的新 executable area。
    /// @param source segment bytes 的唯一随机读来源。
    /// @param offset PT_LOAD 在 source 中的起始 byte offset。
    /// @param size 需要复制的 p_filesz。
    /// @return 映射、填充并发布成功后返回 unit。
    /// @errors 地址冲突、frame/page-table OOM、非法容量或 source I/O error。
    fn push_from_source(
        &mut self,
        mut area: MapArea,
        source: &dyn ExecutableSource,
        offset: usize,
        size: usize,
    ) -> Result<(), ElfLoadError> {
        let start = area.vpn_range.start;
        let end = area.vpn_range.end;
        if self
            .areas
            .values()
            .any(|existing| start < existing.vpn_range.end && existing.vpn_range.start < end)
        {
            return Err(ElfLoadError::InvalidElf);
        }
        area.map(&mut self.page_table).map_err(ElfLoadError::from)?;
        if let Err(error) = area.copy_from_source(source, offset, size) {
            area.unmap(&mut self.page_table);
            return Err(error);
        }
        self.areas.insert(start, area);
        Ok(())
    }

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
            self.push_from_source(
                MapArea::elf(start.into(), end.into(), permission),
                image.source.as_ref(),
                segment.file_offset,
                segment.file_size,
            )?;
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
