//! 由已解析的 ELF 映射计划构造新地址空间。

use alloc::vec::Vec;

use super::executable::{ElfKind, ExecutableImage, ParsedElf};
use super::initial_stack::{ElfAuxInfo, InitialStack};
use super::mm::{AnonymousRole, Area, AreaFlags, Backing, FileMappingSource};
use super::{ElfLoadError, MapPermission, MemoryObject, MemorySet, PAGE_SIZE};

/// PIE 主程序的装载基址。
const MAIN_PIE_BASE: usize = 0x1_0000;
/// 动态解释器的装载基址。
const INTERPRETER_BASE: usize = 0x2000_0000;
/// 栈与 mmap 区之间的最小间隙（Linux `mmap_base` 下限 128 MiB）。
const MMAP_GAP_MIN: usize = 128 * 1024 * 1024;
/// 主线程栈保留的上限（`RLIMIT_STACK` 为无穷时采用）。
const STACK_RESERVE_MAX: usize = 1024 * 1024 * 1024;
/// 主线程栈保留的下限。
const STACK_RESERVE_MIN: usize = 128 * 1024;

struct LoadedElf {
    entry: usize,
    phdr: Option<usize>,
    phent: usize,
    phnum: usize,
    max_end: usize,
}

fn align_down(value: usize) -> usize {
    value & !(PAGE_SIZE - 1)
}

fn align_up(value: usize) -> Result<usize, ElfLoadError> {
    value
        .checked_next_multiple_of(PAGE_SIZE)
        .ok_or(ElfLoadError::InvalidElf)
}

impl MemorySet {
    /// 从已校验的 ELF 计划构造新地址空间（新的成员进程）、初始栈与入口。
    ///
    /// # Errors
    ///
    /// ELF、映射、来源 I/O 或资源失败；未发布的新地址空间随错误释放。
    pub(crate) fn from_elf(
        image: &ExecutableImage,
        stack: InitialStack<'_>,
        stack_limit: u64,
        address_space_limit: u64,
        data_limit: u64,
        name: &[u8],
    ) -> Result<(Self, usize, usize), ElfLoadError> {
        let mut memory = Self::try_new(name)?;
        memory.executable_owner = Some(image.owner.clone());
        memory.interpreter_owner = image.interpreter_owner.clone();
        let main_bias = match image.main.kind {
            ElfKind::Executable => 0,
            ElfKind::SharedObject => MAIN_PIE_BASE,
        };
        let main = memory.map_elf_image(&image.main, main_bias)?;
        let (entry, interpreter_base) = match &image.interpreter {
            Some(interpreter) => {
                if interpreter.kind != ElfKind::SharedObject {
                    return Err(ElfLoadError::InvalidElf);
                }
                (
                    memory.map_elf_image(interpreter, INTERPRETER_BASE)?.entry,
                    INTERPRETER_BASE,
                )
            }
            None => (main.entry, 0),
        };
        let phdr = main.phdr.ok_or(ElfLoadError::InvalidElf)?;
        if memory
            .area_at(phdr)
            .is_none_or(|area| !area.permission.contains(MapPermission::R))
        {
            return Err(ElfLoadError::InvalidElf);
        }

        // 栈：跳板页之下保留一页间隙，按 RLIMIT_STACK 一次保留（页按需提交，无需增长）。
        let stack_size = usize::try_from(stack_limit)
            .unwrap_or(usize::MAX)
            .clamp(STACK_RESERVE_MIN, STACK_RESERVE_MAX)
            & !(PAGE_SIZE - 1);
        let stack_top = memory.trampoline() - PAGE_SIZE;
        let stack_bottom = stack_top
            .checked_sub(stack_size)
            .ok_or(ElfLoadError::InvalidElf)?;
        memory.insert(Area {
            start: stack_bottom,
            end: stack_top,
            permission: MapPermission::U | MapPermission::R | MapPermission::W,
            backing: Backing::Anonymous {
                object: MemoryObject::anonymous(stack_size, true)?,
                offset: 0,
                role: AnonymousRole::Stack,
            },
            flags: AreaFlags::empty(),
            name: None,
        })?;
        memory.stack = stack_bottom..stack_top;
        memory.mmap_top = align_down(
            memory
                .end
                .saturating_sub(stack_size.max(MMAP_GAP_MIN) + PAGE_SIZE),
        );
        memory.initialize_heap(align_up(main.max_end)?)?;

        let aux = ElfAuxInfo {
            phdr,
            phent: main.phent,
            phnum: main.phnum,
            entry: main.entry,
            base: interpreter_base,
        };
        let stack_pointer = memory.build_initial_stack(stack_top, stack, aux, stack_limit)?;
        if memory.virtual_bytes() > address_space_limit || memory.data_bytes() > data_limit {
            return Err(ElfLoadError::OutOfMemory);
        }
        Ok((memory, stack_pointer, entry))
    }

    /// 映射一个 ELF 的全部 `PT_LOAD` 段。
    fn map_elf_image(&mut self, image: &ParsedElf, bias: usize) -> Result<LoadedElf, ElfLoadError> {
        let header_end = image
            .program_header_offset
            .checked_add(image.program_header_entry_size * image.program_header_count)
            .ok_or(ElfLoadError::InvalidElf)?;
        let mapping = image.source.shared_mapping();
        let mut max_end = 0usize;
        let mut phdr = None;
        for segment in &image.load_segments {
            let start = bias
                .checked_add(segment.virtual_address)
                .ok_or(ElfLoadError::InvalidElf)?;
            let end = start
                .checked_add(segment.memory_size)
                .ok_or(ElfLoadError::InvalidElf)?;
            if start < PAGE_SIZE || start >= end || end > self.trampoline() {
                return Err(ElfLoadError::InvalidElf);
            }
            if segment.file_offset % PAGE_SIZE != start % PAGE_SIZE {
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
            let page_start = align_down(start);
            let file_end = start + segment.file_size;
            let file_pages_end = align_up(file_end)?;
            let page_end = align_up(end)?;
            if segment.file_size != 0 {
                let file_page_offset = align_down(segment.file_offset);
                let length = file_pages_end - page_start;
                let backing = match &mapping {
                    Some(shared) => {
                        let object = shared
                            .object()?
                            .private_view(file_page_offset as u64, length as u64)?;
                        // 最后一个文件页中越过 p_filesz 的部分属于 bss：在私有副本中清零。
                        if segment.memory_size > segment.file_size && file_end < file_pages_end {
                            let zeros = [0u8; PAGE_SIZE];
                            object
                                .vmo()
                                .write(
                                    &zeros[..file_pages_end - file_end],
                                    (file_end - page_start) as u64,
                                )
                                .map_err(|_| ElfLoadError::Io)?;
                        }
                        Backing::PrivateFile {
                            source: FileMappingSource {
                                mapping: shared.clone(),
                                offset: file_page_offset as u64,
                            },
                            object,
                            offset: 0,
                            file_offset: file_page_offset as u64,
                        }
                    }
                    None => {
                        let object = MemoryObject::anonymous(length, false)?;
                        let mut bytes = Vec::new();
                        bytes
                            .try_reserve_exact(segment.file_size)
                            .map_err(|_| ElfLoadError::OutOfMemory)?;
                        bytes.resize(segment.file_size, 0);
                        image
                            .source
                            .read_exact_at(segment.file_offset, &mut bytes)
                            .map_err(|()| ElfLoadError::Io)?;
                        object
                            .vmo()
                            .write(&bytes, (start - page_start) as u64)
                            .map_err(|_| ElfLoadError::OutOfMemory)?;
                        Backing::Anonymous {
                            object,
                            offset: 0,
                            role: AnonymousRole::Plain,
                        }
                    }
                };
                self.replace_with(Area {
                    start: page_start,
                    end: file_pages_end,
                    permission,
                    backing,
                    flags: AreaFlags::empty(),
                    name: None,
                })?;
            }
            let anonymous_start = if segment.file_size == 0 {
                page_start
            } else {
                file_pages_end
            };
            if page_end > anonymous_start {
                self.replace_with(Area {
                    start: anonymous_start,
                    end: page_end,
                    permission,
                    backing: Backing::Anonymous {
                        object: MemoryObject::anonymous(page_end - anonymous_start, false)?,
                        offset: 0,
                        role: AnonymousRole::Plain,
                    },
                    flags: AreaFlags::empty(),
                    name: None,
                })?;
            }
            max_end = max_end.max(end);
            if segment.file_offset <= image.program_header_offset
                && header_end <= segment.file_offset + segment.file_size
            {
                phdr = start.checked_add(image.program_header_offset - segment.file_offset);
            }
        }
        let entry = bias
            .checked_add(image.entry)
            .ok_or(ElfLoadError::InvalidElf)?;
        if self
            .area_at(entry)
            .is_none_or(|area| !area.permission.contains(MapPermission::X))
        {
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

    /// 以 `area` 覆盖其区间内已有映射（段共享页时后者覆盖前者）。
    fn replace_with(&mut self, area: Area) -> Result<(), ElfLoadError> {
        self.remove_range(area.start, area.end)?;
        self.insert(area)?;
        Ok(())
    }
}
