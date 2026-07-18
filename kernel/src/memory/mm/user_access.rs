use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// @description 单次用户缺页可使用的 Process 级虚拟内存资源边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UserFaultLimits {
    pub(crate) stack: u64,
    pub(crate) address_space: u64,
}

impl UserFaultLimits {
    /// @description 构造来自当前 Process `RLIMIT_STACK/RLIMIT_AS` 的 fault 边界。
    ///
    /// @param stack 最大 grow-down stack 字节数。
    /// @param address_space 最大用户 VMA 总字节数。
    /// @return 可在一次 fault/copy transaction 内复用的不可变限制快照。
    pub(crate) const fn new(stack: u64, address_space: u64) -> Self {
        Self {
            stack,
            address_space,
        }
    }

    /// @description 为只允许命中既有 VMA 的内核读取构造边界，不允许隐式扩栈。
    pub(super) const fn existing_mappings() -> Self {
        Self::new(0, u64::MAX)
    }

    /// @description 为 exec transaction 构造固定初始栈边界。
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

    /// @description Fault-in 并验证完整用户读取范围，不复制内容。
    ///
    /// @param address 用户范围首地址。
    /// @param length 用户范围字节数。
    /// @param limits lazy fault 可消耗的资源上限。
    /// @return 完整范围已驻留且具备 `U|R` 权限时返回 exclusive end。
    /// @errors 地址、权限、fault 或资源失败返回 `UserAccessError`。
    pub(super) fn prepare_user_read(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(address, length)?;
        let mut current = address;
        while current < end {
            if self.user_page(current, PagePermissions::READ).is_err() {
                self.fault_in_user_page(current, PageFaultAccess::Read, limits)?;
            }
            current = (current | (config::PAGE_SIZE - 1))
                .saturating_add(1)
                .min(end);
        }
        Ok(end)
    }

    /// @description 完成 COW/dirty 转换并验证完整用户写入范围，不复制内容。
    ///
    /// @param address 用户范围首地址。
    /// @param length 用户范围字节数。
    /// @param limits lazy fault 可消耗的资源上限。
    /// @return 完整范围已驻留且具备 `U|W` 权限时返回 exclusive end。
    /// @errors 地址、权限、fault 或资源失败返回 `UserAccessError`。
    fn prepare_user_write(
        &mut self,
        address: usize,
        length: usize,
        limits: UserFaultLimits,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(address, length)?;
        let mut current = address;
        while current < end {
            // 1. write fault domain 唯一提交 COW、MADV_FREE 取消和 private-file dirty；
            // 绕过它直接按 PTE copy 会让共享 frame 被覆盖或让已写页仍可被回收。
            self.fault_in_user_page(current, PageFaultAccess::Write, limits)?;
            // 2. Handled 的领域 postcondition 已证明 U|W leaf 可重试；再次查询只会让每个
            // copyout 页多走一遍 architecture page table，不提供新的失败原子性或状态证明。
            current = (current | (config::PAGE_SIZE - 1))
                .saturating_add(1)
                .min(end);
        }
        Ok(end)
    }

    /// @description 完整 fault/校验后，从用户页复制到 kernel-owned 缓冲区。
    pub(crate) fn copy_from_user(
        &mut self,
        address: usize,
        destination: &mut [u8],
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
                    destination.as_mut_ptr().add(copied),
                    count,
                )
            };
            current += count;
            copied += count;
        }
        Ok(())
    }

    /// @description 完整解析 lazy/COW 页后，将 kernel-owned 字节复制到用户页。
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
        if address & 3 != 0 {
            return Err(UserAccessError::Fault);
        }
        self.prepare_user_write(address, 4, limits)?;
        let (ppn, offset) =
            self.user_page(address, PagePermissions::READ | PagePermissions::WRITE)?;
        if offset + 4 > config::PAGE_SIZE {
            return Err(UserAccessError::Fault);
        }
        // SAFETY: live U|R|W page、alignment 与页内边界均已验证，AddressSpace lock 保持映射稳定。
        let atomic = unsafe { &*ppn.as_page_ptr().add(offset).cast::<AtomicU32>() };
        Ok(atomic.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire))
    }

    /// @description 从用户空间复制有上限的 NUL 结尾字节串。
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
