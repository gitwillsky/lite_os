use alloc::vec::Vec;
use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, Ordering},
};

use super::*;

/// 单次用户缺页可使用的 Process 级虚拟内存资源边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserFaultLimits {
    pub(crate) stack: u64,
    pub(crate) address_space: u64,
}

impl UserFaultLimits {
    /// 构造来自当前 Process `RLIMIT_STACK/RLIMIT_AS` 的 fault 边界。
    ///
    /// # Parameters
    ///
    /// - `stack`: 最大 grow-down stack 字节数。
    /// - `address_space`: 最大用户 VMA 总字节数。
    ///
    /// # Returns
    ///
    /// 可在一次 fault/copy transaction 内复用的不可变限制快照。
    pub(crate) const fn new(stack: u64, address_space: u64) -> Self {
        Self {
            stack,
            address_space,
        }
    }

    /// 为只允许命中既有 VMA 的内核读取构造边界，不允许隐式扩栈。
    pub(super) const fn existing_mappings() -> Self {
        Self::new(0, u64::MAX)
    }

    /// 为 exec transaction 构造固定初始栈边界。
    pub(super) const fn initial_exec() -> Self {
        Self::new(config::USER_STACK_SIZE as u64, u64::MAX)
    }
}

impl MemorySet {
    pub(super) fn checked_user_end(start: usize, len: usize) -> Result<usize, UserAccessError> {
        if len == 0 {
            return Ok(start);
        }
        let end = start.checked_add(len).ok_or(UserAccessError::Overflow)?;
        let user_end = config::USER_ADDRESS_END;
        if start == 0 || start >= user_end || end > user_end {
            return Err(UserAccessError::Fault);
        }
        Ok(end)
    }

    pub(super) fn user_page(
        &self,
        address: usize,
        required: PagePermissions,
    ) -> Result<(PhysicalPageNumber, usize), UserAccessError> {
        let va = VirtualAddress::from(address);
        let pte = self
            .page_table
            .translate(va.floor())
            .ok_or(UserAccessError::Fault)?;
        if !pte.permissions().contains(PagePermissions::USER | required) {
            return Err(UserAccessError::Fault);
        }
        Ok((pte.ppn(), va.page_offset()))
    }

    fn fault_in_user_page(
        &mut self,
        address: usize,
        access: PageFaultAccess,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        match self.handle_page_fault_with_limits(address, access, limits) {
            Ok(PageFaultOutcome::Handled) => Ok(()),
            Err(MemoryError::OutOfMemory) => Err(UserAccessError::OutOfMemory),
            _ => Err(UserAccessError::Fault),
        }
    }

    /// Fault-in 并验证完整用户读取范围，不复制内容。
    ///
    /// # Parameters
    ///
    /// - `address`: 用户范围首地址。
    /// - `length`: 用户范围字节数。
    /// - `limits`: lazy fault 可消耗的资源上限。
    ///
    /// # Returns
    ///
    /// 完整范围已驻留且具备 `U|R` 权限时返回 exclusive end。
    ///
    /// # Errors
    ///
    /// 地址、权限、fault 或资源失败返回 `UserAccessError`。
    pub(super) fn prepare_user_read(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<usize, UserAccessError> {
        self.prepare_user_range(address, length, PageFaultAccess::Read, limits)
    }

    /// 完成 COW/dirty 转换并验证完整用户写入范围，不复制内容。
    fn prepare_user_write(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<usize, UserAccessError> {
        self.prepare_user_range(address, length, PageFaultAccess::Write, limits)
    }

    /// 在唯一 mm owner 内排除整个准备范围，防止后续 fault 的自身回收逐出先前页。
    fn prepare_user_range(
        &mut self,
        address: usize,
        length: usize,
        access: PageFaultAccess,
        limits: UserFaultLimits,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(address, length)?;
        if length == 0 {
            return Ok(end);
        }
        let range = VirtualAddress::from(address).floor()..VirtualAddress::from(end).ceil();
        let previous = self.private_reclaim_exclusion.replace(range);
        assert!(
            previous.is_none(),
            "user-copy preparation must not reenter the same mm"
        );
        let prepared = (|| {
            let mut current = address;
            while current < end {
                // write 必须经过 fault owner 取消 MADV_FREE/提交 private-file dirty；
                // read 的现存可读 leaf 无需再次 fault 或增加 translation fence。
                if access == PageFaultAccess::Write
                    || self.user_page(current, PagePermissions::READ).is_err()
                {
                    self.fault_in_user_page(current, access, limits)?;
                }
                current = (current | (config::PAGE_SIZE - 1))
                    .saturating_add(1)
                    .min(end);
            }
            Ok(end)
        })();
        // 成功/错误均恢复；后续 copy 无 allocation/fault，且同一 AddressSpace lock
        // 持续阻止外部 reclaim，所以 preparation 的 residency 证明保持到复制结束。
        self.private_reclaim_exclusion = previous;
        prepared
    }

    /// 完整 fault/校验后，从用户页复制到 kernel-owned 缓冲区。
    pub(crate) fn copy_from_user(
        &mut self,
        address: usize,
        destination: &mut [u8],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        // SAFETY: initialized u8 storage may always be viewed as MaybeUninit<u8>; the delegated
        // operation writes every element before returning success and never changes slice layout.
        let destination = unsafe {
            core::slice::from_raw_parts_mut(
                destination.as_mut_ptr().cast::<MaybeUninit<u8>>(),
                destination.len(),
            )
        };
        self.copy_from_user_uninit(address, destination, limits)
    }

    /// 完整 fault/校验后直接初始化 kernel destination，避免预清零 staging。
    ///
    /// # Parameters
    ///
    /// - `address`: 用户源范围首地址。
    /// - `destination`: 尚未初始化、由 caller 独占的 kernel byte storage。
    /// - `limits`: fault 可消耗的资源上限。
    ///
    /// # Returns
    ///
    /// 成功时 destination 全部初始化；失败时 caller 不得读取其内容。
    pub(crate) fn copy_from_user_uninit(
        &mut self,
        address: usize,
        destination: &mut [MaybeUninit<u8>],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        let end = self.prepare_user_read(address, destination.len(), limits)?;
        let mut current = address;
        let mut copied = 0;
        while current < end {
            let (ppn, offset) = self.user_page(current, PagePermissions::READ)?;
            let count = (config::PAGE_SIZE - offset).min(end - current);
            // SAFETY: user_page 证明源页存活且 U|R；destination 是有效独占切片。
            unsafe {
                core::ptr::copy(
                    ppn.as_page_ptr().add(offset),
                    destination.as_mut_ptr().cast::<u8>().add(copied),
                    count,
                )
            };
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// 从 U|X mapping 精确读取一个 RISC-V instruction halfword。
    ///
    /// # Parameters
    ///
    /// - `address`: 2-byte aligned 用户 instruction address。
    /// - `destination`: 固定两字节 kernel buffer。
    ///
    /// # Returns
    ///
    /// execute fault 完成且 U|X leaf 可读时成功；权限、地址或资源错误时失败。
    pub(crate) fn copy_instruction_halfword(
        &mut self,
        address: usize,
        destination: &mut [u8; 2],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        let end = Self::checked_user_end(address, destination.len())?;
        let mut current = address;
        let mut copied = 0;
        while current < end {
            if self.user_page(current, PagePermissions::EXECUTE).is_err() {
                self.fault_in_user_page(current, PageFaultAccess::Execute, limits)?;
            }
            let (ppn, offset) = self.user_page(current, PagePermissions::EXECUTE)?;
            let count = (config::PAGE_SIZE - offset).min(end - current);
            // SAFETY: user_page 证明源页存活且 U|X；读取 instruction image 不要求 U|R。
            unsafe {
                core::ptr::copy(
                    ppn.as_page_ptr().add(offset),
                    destination.as_mut_ptr().add(copied),
                    count,
                )
            };
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// 完整解析 lazy/COW 页后，将 kernel-owned 字节复制到用户页。
    pub(crate) fn copy_to_user(
        &mut self,
        address: usize,
        source: &[u8],
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        let end = self.prepare_user_write(address, source.len(), limits)?;
        let mut current = address;
        let mut copied = 0;
        while current < end {
            let (ppn, offset) = self.user_page(current, PagePermissions::WRITE)?;
            let count = (config::PAGE_SIZE - offset).min(end - current);
            // SAFETY: user_page 证明目标页存活且 U|W；source 是有效只读切片。
            unsafe {
                core::ptr::copy(
                    source.as_ptr().add(copied),
                    ppn.as_page_mut_ptr().add(offset),
                    count,
                )
            };
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// 完整 fault/校验一次用户范围，并直接把目标页内容清零。
    ///
    /// # Parameters
    ///
    /// - `address`: 用户目标起点。
    /// - `length`: 清零字节数。
    /// - `limits`: lazy/COW fault 可使用的 Process 资源边界。
    ///
    /// # Returns
    ///
    /// 完整范围清零成功返回 Ok。
    ///
    /// # Errors
    ///
    /// 地址、权限、fault、overflow 或资源失败返回 `UserAccessError`。
    pub(crate) fn zero_user(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        let end = self.prepare_user_write(address, length, limits)?;
        let mut current = address;
        while current < end {
            let (ppn, offset) = self.user_page(current, PagePermissions::WRITE)?;
            let count = (config::PAGE_SIZE - offset).min(end - current);
            // SAFETY: user_page 证明目标页存活且 U|W；范围不跨页，write_bytes 不读取旧内容。
            unsafe { core::ptr::write_bytes(ppn.as_page_mut_ptr().add(offset), 0, count) };
            current += count;
        }
        Ok(())
    }

    pub(crate) fn validate_user_write(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<(), UserAccessError> {
        self.prepare_user_write(address, length, limits).map(|_| ())
    }

    pub(crate) fn compare_exchange_user_u32(
        &mut self,
        address: usize,
        current: u32,
        new: u32,
        limits: UserFaultLimits,
    ) -> Result<Result<u32, u32>, UserAccessError> {
        Ok(self
            .futex_atomic_word(address, limits)?
            .compare_exchange(current, new))
    }

    /// native get_user(u32) 的单次 aligned load；不能逐 byte copy 出撕裂的 futex value。
    pub(crate) fn load_user_u32(
        &mut self,
        address: usize,
        limits: UserFaultLimits,
    ) -> Result<u32, UserAccessError> {
        if address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        self.prepare_user_read(address, 4, limits)?;
        let (ppn, offset) = self.user_page(address, PagePermissions::READ)?;
        // SAFETY: 已验证 live U|R、alignment 与完整页内范围；MM guard 保持映射和物理页存活。
        Ok(unsafe { &*ppn.as_page_ptr().add(offset).cast::<AtomicU32>() }.load(Ordering::Acquire))
    }

    /// 为 WAKE_OP 借用实际可写原子 word；MM guard 保持映射与物理页存活。
    pub(crate) fn futex_atomic_word(
        &mut self,
        address: usize,
        limits: UserFaultLimits,
    ) -> Result<UserAtomicU32<'_>, UserAccessError> {
        if address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        self.prepare_user_write(address, 4, limits)?;
        let (ppn, offset) = self.user_page(address, PagePermissions::WRITE)?;
        // SAFETY: 已验证 U|W、4-byte alignment 和整段页内范围；返回借用不得越过 MM owner。
        Ok(UserAtomicU32(unsafe {
            &*ppn.as_page_ptr().add(offset).cast::<AtomicU32>()
        }))
    }

    /// 从用户空间复制有上限的 NUL 结尾字节串。
    pub(crate) fn copy_user_c_string(
        &mut self,
        address: usize,
        max_len: usize,
        limits: UserFaultLimits,
    ) -> Result<Vec<u8>, UserAccessError> {
        if address == 0 {
            return Err(UserAccessError::Fault);
        }
        let mut bytes = Vec::new();
        let mut current = address;
        while bytes.len() < max_len {
            Self::checked_user_end(current, 1)?;
            if self.user_page(current, PagePermissions::READ).is_err() {
                self.fault_in_user_page(current, PageFaultAccess::Read, limits)?;
            }
            let (ppn, offset) = self.user_page(current, PagePermissions::READ)?;
            let count = (config::PAGE_SIZE - offset).min(max_len - bytes.len());
            // SAFETY: user_page 证明当前页存活且可读，slice 不越页且不逃逸本次循环。
            let page = unsafe { core::slice::from_raw_parts(ppn.as_page_ptr().add(offset), count) };
            if let Some(nul) = page.iter().position(|byte| *byte == 0) {
                bytes
                    .try_reserve_exact(nul)
                    .map_err(|_| UserAccessError::OutOfMemory)?;
                bytes.extend_from_slice(&page[..nul]);
                return Ok(bytes);
            }
            bytes
                .try_reserve_exact(count)
                .map_err(|_| UserAccessError::OutOfMemory)?;
            bytes.extend_from_slice(page);
            current = current
                .checked_add(count)
                .ok_or(UserAccessError::Overflow)?;
        }
        Err(UserAccessError::Unterminated)
    }
}
