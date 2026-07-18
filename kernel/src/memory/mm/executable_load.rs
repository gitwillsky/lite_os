use alloc::vec::Vec;

use crate::memory::{
    address::VirtualAddress,
    config,
    executable::{ElfKind, ExecutableImage, ParsedElf},
    page_table::PagePermissions,
};

use super::{
    ElfLoadError, LoadedElf, MapArea, MapPermission, MapType, MemorySet, PageFaultAccess,
    PageFaultOutcome, PrivateFileArea, initial_stack::ElfAuxInfo,
};

impl MemorySet {
    /// @description 从已校验 ELF plan 构造受 rlimit 约束的新地址空间、初始栈与 entry。
    pub(crate) fn from_elf(
        image: &ExecutableImage,
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
        execfn: &[u8],
        stack_limit: u64,
        address_space_limit: u64,
        data_limit: u64,
    ) -> Result<(Self, usize, usize), ElfLoadError> {
        let mut memory_set = MemorySet::try_new().map_err(ElfLoadError::from)?;
        memory_set.map_trampoline().map_err(ElfLoadError::from)?;
        const MAIN_PIE_BASE: usize = 0x1_0000;
        const INTERPRETER_BASE: usize = 0x2000_0000;
        let main_type = image.main.kind;
        let main_bias = match main_type {
            ElfKind::Executable if image.interpreter.is_none() => 0,
            ElfKind::SharedObject if image.interpreter.is_some() => MAIN_PIE_BASE,
            _ => return Err(ElfLoadError::InvalidElf),
        };
        let main = memory_set.map_elf_image(&image.main, main_bias)?;
        memory_set.code_range = main.code_range.clone();
        let (entry_point, interpreter_base) = if let Some(interpreter) = &image.interpreter {
            if interpreter.kind != ElfKind::SharedObject {
                return Err(ElfLoadError::InvalidElf);
            }
            let loaded = memory_set.map_elf_image(interpreter, INTERPRETER_BASE)?;
            (loaded.entry, INTERPRETER_BASE)
        } else {
            (main.entry, 0)
        };
        let phdr_address = main.phdr.ok_or(ElfLoadError::InvalidElf)?;
        let heap_base = VirtualAddress::from(main.max_end).ceil().as_usize() * config::PAGE_SIZE;
        let user_end = config::USER_ADDRESS_END;
        let user_stack_top = user_end
            .checked_sub(config::PAGE_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        let heap_limit = user_stack_top
            .checked_sub(config::PAGE_SIZE)
            .ok_or(ElfLoadError::InvalidElf)?;
        if heap_base >= heap_limit {
            return Err(ElfLoadError::InvalidElf);
        }

        // 2. heap 从最高 LOAD 末端开始；栈位于 architecture user range 顶部，上下各保留一页 guard。
        memory_set
            .push(MapArea::stack(user_stack_top), None)
            .map_err(ElfLoadError::from)?;
        memory_set
            .initialize_user_heap(heap_base, heap_limit)
            .map_err(ElfLoadError::from)?;
        memory_set
            .push(
                MapArea::new(
                    config::TRAP_CONTEXT.into(),
                    config::TRAMPOLINE.into(),
                    MapType::Framed,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .map_err(ElfLoadError::from)?;

        if memory_set.handle_page_fault(phdr_address, PageFaultAccess::Read)?
            != PageFaultOutcome::Handled
        {
            return Err(ElfLoadError::InvalidElf);
        }
        let phdr_pte = memory_set
            .translate(VirtualAddress::from(phdr_address).floor())
            .ok_or(ElfLoadError::InvalidElf)?;
        if !phdr_pte
            .permissions()
            .contains(PagePermissions::USER | PagePermissions::READ)
        {
            return Err(ElfLoadError::InvalidElf);
        }

        // 3. 初始栈是 argv/envp/auxv 的唯一用户契约，不通过寄存器传递私有参数。
        let aux = ElfAuxInfo::new(
            phdr_address,
            main.phent,
            main.phnum,
            main.entry,
            interpreter_base,
        );
        let actual_stack_top =
            memory_set.build_initial_stack(user_stack_top, args, envs, execfn, aux, stack_limit)?;
        if memory_set.virtual_bytes() > address_space_limit || memory_set.data_bytes() > data_limit
        {
            return Err(ElfLoadError::OutOfMemory);
        }
        Ok((memory_set, actual_stack_top, entry_point))
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
        let mut code_start = usize::MAX;
        let mut code_end = 0usize;
        let mut phdr = None;
        for segment in &image.load_segments {
            let start = load_bias
                .checked_add(segment.virtual_address)
                .ok_or(ElfLoadError::InvalidElf)?;
            let end = start
                .checked_add(segment.memory_size)
                .ok_or(ElfLoadError::InvalidElf)?;
            let user_end = config::USER_ADDRESS_END;
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
                code_start = code_start.min(start);
                code_end = code_end.max(
                    start
                        .checked_add(segment.file_size)
                        .ok_or(ElfLoadError::InvalidElf)?,
                );
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
        if !entry_pte
            .permissions()
            .contains(PagePermissions::USER | PagePermissions::EXECUTE)
        {
            return Err(ElfLoadError::InvalidElf);
        }
        Ok(LoadedElf {
            entry,
            phdr,
            phent: image.program_header_entry_size,
            phnum: image.program_header_count,
            max_end,
            code_range: code_start..code_end,
        })
    }
}
