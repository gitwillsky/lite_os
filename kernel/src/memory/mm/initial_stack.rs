use alloc::vec::Vec;

use crate::memory::config;

use super::{ElfLoadError, MemorySet};

#[derive(Debug, Clone, Copy)]
pub(super) struct ElfAuxInfo {
    phdr: usize,
    phent: usize,
    phnum: usize,
    entry: usize,
    base: usize,
}

impl ElfAuxInfo {
    /// @description 组合 main ELF 与 optional interpreter 的 Linux auxv facts。
    ///
    /// @param phdr AT_PHDR virtual address。
    /// @param phent AT_PHENT entry size。
    /// @param phnum AT_PHNUM entry count。
    /// @param entry AT_ENTRY main ELF entry。
    /// @param base AT_BASE interpreter load bias；static executable 为零。
    /// @return immutable initial-stack auxv input。
    pub(super) fn new(phdr: usize, phent: usize, phnum: usize, entry: usize, base: usize) -> Self {
        Self {
            phdr,
            phent,
            phnum,
            entry,
            base,
        }
    }
}

impl MemorySet {
    /// @description 构造 Linux RV64 argc/argv/envp/auxv 初始栈，并保持 16-byte alignment。
    ///
    /// @param stack_top 已映射用户栈的 exclusive upper bound。
    /// @param args script rewrite 后且不含 NUL 的 argv strings。
    /// @param envs 不含 NUL 的 envp strings。
    /// @param execfn 用户传给 execve 的原始 pathname。
    /// @param aux 最终 main ELF 与 interpreter 产生的 auxv facts。
    /// @return 16-byte aligned initial stack pointer。
    /// @errors stack size/地址无效、user copy、entropy 或 allocation 失败。
    pub(super) fn build_initial_stack(
        &mut self,
        stack_top: usize,
        args: &[Vec<u8>],
        envs: &[Vec<u8>],
        execfn: &[u8],
        aux: ElfAuxInfo,
    ) -> Result<usize, ElfLoadError> {
        const AT_NULL: usize = 0;
        const AT_PHDR: usize = 3;
        const AT_PHENT: usize = 4;
        const AT_PHNUM: usize = 5;
        const AT_PAGESZ: usize = 6;
        const AT_BASE: usize = 7;
        const AT_ENTRY: usize = 9;
        const AT_RANDOM: usize = 25;
        const AT_EXECFN: usize = 31;
        const AUX_WORDS: usize = 18;
        const RANDOM_BYTES: usize = 16;

        let total_string_size = args
            .iter()
            .chain(envs)
            .try_fold(0usize, |total, value| {
                value
                    .len()
                    .checked_add(1)
                    .and_then(|size| total.checked_add(size))
            })
            .and_then(|size| {
                execfn
                    .len()
                    .checked_add(1)
                    .and_then(|execfn_size| size.checked_add(execfn_size))
            })
            .and_then(|size| size.checked_add(RANDOM_BYTES))
            .ok_or(ElfLoadError::InvalidElf)?;
        let pointer_count = 1usize
            .checked_add(args.len())
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_add(envs.len()))
            .and_then(|count| count.checked_add(1))
            .and_then(|count| count.checked_add(AUX_WORDS))
            .ok_or(ElfLoadError::InvalidElf)?;
        let pointer_space = pointer_count
            .checked_mul(core::mem::size_of::<usize>())
            .ok_or(ElfLoadError::InvalidElf)?;
        let stack_size = pointer_space
            .checked_add(total_string_size)
            .and_then(|size| size.checked_add(15))
            .ok_or(ElfLoadError::InvalidElf)?;
        if stack_size > config::USER_STACK_SIZE {
            return Err(ElfLoadError::InvalidElf);
        }
        let stack_ptr = stack_top
            .checked_sub(stack_size)
            .ok_or(ElfLoadError::InvalidElf)?
            & !15usize;
        let mut string_ptr = stack_ptr
            .checked_add(pointer_space)
            .ok_or(ElfLoadError::InvalidElf)?;
        let mut argv_ptrs = Vec::new();
        argv_ptrs
            .try_reserve_exact(args.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        let mut envp_ptrs = Vec::new();
        envp_ptrs
            .try_reserve_exact(envs.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;

        for arg in args {
            argv_ptrs.push(string_ptr);
            self.write_c_string_to_user_stack(string_ptr, arg)?;
            string_ptr = string_ptr
                .checked_add(arg.len())
                .and_then(|address| address.checked_add(1))
                .ok_or(ElfLoadError::InvalidElf)?;
        }
        for env in envs {
            envp_ptrs.push(string_ptr);
            self.write_c_string_to_user_stack(string_ptr, env)?;
            string_ptr = string_ptr
                .checked_add(env.len())
                .and_then(|address| address.checked_add(1))
                .ok_or(ElfLoadError::InvalidElf)?;
        }
        let execfn_ptr = string_ptr;
        self.write_c_string_to_user_stack(execfn_ptr, execfn)?;
        string_ptr = string_ptr
            .checked_add(execfn.len())
            .and_then(|address| address.checked_add(1))
            .ok_or(ElfLoadError::InvalidElf)?;
        let random_ptr = string_ptr;
        let mut random = [0u8; RANDOM_BYTES];
        crate::random::fill(&mut random).map_err(|_| ElfLoadError::InvalidElf)?;
        self.copy_to_user(random_ptr, &random)
            .map_err(|_| ElfLoadError::InvalidElf)?;

        let mut writer = stack_ptr;
        self.write_usize_to_user_stack(writer, args.len())?;
        writer += core::mem::size_of::<usize>();
        for pointer in argv_ptrs {
            self.write_usize_to_user_stack(writer, pointer)?;
            writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(writer, 0)?;
        writer += core::mem::size_of::<usize>();
        for pointer in envp_ptrs {
            self.write_usize_to_user_stack(writer, pointer)?;
            writer += core::mem::size_of::<usize>();
        }
        self.write_usize_to_user_stack(writer, 0)?;
        writer += core::mem::size_of::<usize>();

        for (kind, value) in [
            (AT_PHDR, aux.phdr),
            (AT_PHENT, aux.phent),
            (AT_PHNUM, aux.phnum),
            (AT_PAGESZ, config::PAGE_SIZE),
            (AT_BASE, aux.base),
            (AT_ENTRY, aux.entry),
            (AT_RANDOM, random_ptr),
            (AT_EXECFN, execfn_ptr),
            (AT_NULL, 0),
        ] {
            self.write_usize_to_user_stack(writer, kind)?;
            writer += core::mem::size_of::<usize>();
            self.write_usize_to_user_stack(writer, value)?;
            writer += core::mem::size_of::<usize>();
        }

        debug_assert_eq!(writer, stack_ptr + pointer_space);
        debug_assert_eq!(stack_ptr & 15, 0);
        Ok(stack_ptr)
    }

    fn write_c_string_to_user_stack(
        &mut self,
        address: usize,
        value: &[u8],
    ) -> Result<(), ElfLoadError> {
        self.copy_to_user(address, value)
            .map_err(|_| ElfLoadError::InvalidElf)?;
        let nul_address = address
            .checked_add(value.len())
            .ok_or(ElfLoadError::InvalidElf)?;
        self.copy_to_user(nul_address, &[0])
            .map_err(|_| ElfLoadError::InvalidElf)
    }

    fn write_usize_to_user_stack(
        &mut self,
        address: usize,
        value: usize,
    ) -> Result<(), ElfLoadError> {
        self.copy_to_user(address, &value.to_le_bytes())
            .map_err(|_| ElfLoadError::InvalidElf)
    }
}
