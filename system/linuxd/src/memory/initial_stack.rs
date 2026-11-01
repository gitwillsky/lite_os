//! Linux ELF64 初始栈：argc、argv、envp、auxv 与字符串区。

use alloc::vec::Vec;

use super::{ElfLoadError, MemorySet, PAGE_SIZE};

/// exec 装载流程提供的 Linux 初始栈输入；memory 只负责布局与复制，不访问 entropy device。
pub(crate) struct InitialStack<'a> {
    /// exec 已准备的最终身份与 secureexec，不在 memory 内重算权限。
    pub(crate) credentials: InitialStackCredentials,
    /// script rewrite 后且不含 NUL 的 argv strings。
    pub(crate) arguments: &'a [Vec<u8>],
    /// 不含 NUL 的 envp strings。
    pub(crate) environments: &'a [Vec<u8>],
    /// 用户传给 execve 的原始 pathname。
    pub(crate) execfn: &'a [u8],
    /// 已由 entropy source 完整初始化的 16-byte AT_RANDOM payload。
    pub(crate) random: &'a [u8; 16],
}

/// Linux auxv 身份 facts；只包含已确定的 scalar，不持有 credential owner。
#[derive(Debug, Clone, Copy)]
pub(crate) struct InitialStackCredentials {
    pub(crate) uid: u32,
    pub(crate) euid: u32,
    pub(crate) gid: u32,
    pub(crate) egid: u32,
    pub(crate) secure: bool,
}

/// main ELF 与可选解释器产生的 auxv 事实。
#[derive(Debug, Clone, Copy)]
pub(super) struct ElfAuxInfo {
    pub(super) phdr: usize,
    pub(super) phent: usize,
    pub(super) phnum: usize,
    pub(super) entry: usize,
    /// AT_BASE：解释器装载基址；静态程序为 0。
    pub(super) base: usize,
}

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
const AT_BASE: usize = 7;
const AT_ENTRY: usize = 9;
const AT_UID: usize = 11;
const AT_EUID: usize = 12;
const AT_GID: usize = 13;
const AT_EGID: usize = 14;
const AT_HWCAP: usize = 16;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;
const AT_EXECFN: usize = 31;
const WORD: usize = core::mem::size_of::<usize>();

impl MemorySet {
    /// 构造 Linux ELF64 初始栈并写入用户栈顶，返回 16 字节对齐的初始栈指针。
    ///
    /// 整块栈镜像先在服务内存中构造，再一次复制到用户栈：字符串区紧贴栈顶，指针区在其下。
    ///
    /// # Errors
    ///
    /// 镜像超过 `stack_limit`（`InvalidElf`，与 Linux E2BIG 路径一致由调用者映射）、内存不足或写入失败。
    pub(super) fn build_initial_stack(
        &mut self,
        stack_top: usize,
        input: InitialStack<'_>,
        aux: ElfAuxInfo,
        stack_limit: u64,
    ) -> Result<usize, ElfLoadError> {
        let InitialStack {
            arguments,
            environments,
            execfn,
            random,
            credentials,
        } = input;
        let auxv: [(usize, usize); 15] = [
            (AT_PHDR, aux.phdr),
            (AT_PHENT, aux.phent),
            (AT_PHNUM, aux.phnum),
            (AT_PAGESZ, PAGE_SIZE),
            (AT_BASE, aux.base),
            (AT_ENTRY, aux.entry),
            (AT_HWCAP, crate::arch::user::ELF_HWCAP),
            (AT_UID, credentials.uid as usize),
            (AT_EUID, credentials.euid as usize),
            (AT_GID, credentials.gid as usize),
            (AT_EGID, credentials.egid as usize),
            (AT_SECURE, usize::from(credentials.secure)),
            (AT_RANDOM, 0),
            (AT_EXECFN, 0),
            (AT_NULL, 0),
        ];
        let strings = arguments
            .iter()
            .chain(environments)
            .map(Vec::as_slice)
            .chain(core::iter::once(execfn))
            .try_fold(0usize, |total, value| total.checked_add(value.len() + 1))
            .and_then(|total| total.checked_add(random.len()))
            .ok_or(ElfLoadError::InvalidElf)?;
        let pointers = (1 + arguments.len() + 1 + environments.len() + 1 + auxv.len() * 2)
            .checked_mul(WORD)
            .ok_or(ElfLoadError::InvalidElf)?;
        let stack_pointer = stack_top
            .checked_sub(pointers.checked_add(strings).ok_or(ElfLoadError::InvalidElf)?)
            .ok_or(ElfLoadError::InvalidElf)?
            & !15;
        let size = stack_top - stack_pointer;
        if size as u64 > stack_limit || stack_pointer < self.stack.start {
            return Err(ElfLoadError::InvalidElf);
        }
        let mut image = Vec::new();
        image.try_reserve_exact(size).map_err(|_| ElfLoadError::OutOfMemory)?;
        image.resize(size, 0);
        // 字符串区从指针区之后开始；对齐填充落在两区之间。
        let mut string = stack_pointer + pointers;
        let put_string = |image: &mut Vec<u8>, string: &mut usize, value: &[u8]| -> usize {
            let address = *string;
            let offset = address - stack_pointer;
            image[offset..offset + value.len()].copy_from_slice(value);
            *string += value.len() + 1;
            address
        };
        let mut argument_pointers = Vec::new();
        argument_pointers
            .try_reserve_exact(arguments.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        for argument in arguments {
            argument_pointers.push(put_string(&mut image, &mut string, argument));
        }
        let argument_end = string;
        let mut environment_pointers = Vec::new();
        environment_pointers
            .try_reserve_exact(environments.len())
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        for environment in environments {
            environment_pointers.push(put_string(&mut image, &mut string, environment));
        }
        let execfn_pointer = put_string(&mut image, &mut string, execfn);
        let random_pointer = string;
        let offset = random_pointer - stack_pointer;
        image[offset..offset + random.len()].copy_from_slice(random);

        let mut words = Vec::new();
        words
            .try_reserve_exact(pointers / WORD)
            .map_err(|_| ElfLoadError::OutOfMemory)?;
        words.push(arguments.len());
        words.extend_from_slice(&argument_pointers);
        words.push(0);
        words.extend_from_slice(&environment_pointers);
        words.push(0);
        for (kind, value) in auxv {
            let value = match kind {
                AT_RANDOM => random_pointer,
                AT_EXECFN => execfn_pointer,
                _ => value,
            };
            words.push(kind);
            words.push(value);
        }
        for (index, word) in words.iter().enumerate() {
            image[index * WORD..(index + 1) * WORD].copy_from_slice(&word.to_le_bytes());
        }
        self.copy_to_user(stack_pointer, &image)
            .map_err(|_| ElfLoadError::InvalidElf)?;
        self.arguments = argument_pointers.first().copied().unwrap_or(argument_end)..argument_end;
        Ok(stack_pointer)
    }
}
