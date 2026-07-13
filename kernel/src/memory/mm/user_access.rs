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
        let user_end = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1);
        if start == 0 || start >= user_end || end > user_end {
            return Err(UserAccessError::Fault);
        }
        Ok(end)
    }

    pub(super) fn user_page(
        &self,
        address: usize,
        required: PTEFlags,
    ) -> Result<(PhysicalPageNumber, usize), UserAccessError> {
        let va = VirtualAddress::from(address);
        let pte = self
            .page_table
            .translate(va.floor())
            .ok_or(UserAccessError::Fault)?;
        if !pte.flags().contains(PTEFlags::U | required) {
            return Err(UserAccessError::Fault);
        }
        Ok((pte.ppn(), va.page_offset()))
    }

    pub(super) fn validate_user_range(
        &self,
        start: usize,
        len: usize,
        required: PTEFlags,
    ) -> Result<usize, UserAccessError> {
        let end = Self::checked_user_end(start, len)?;
        let mut current = start;
        while current < end {
            let (_, offset) = self.user_page(current, required)?;
            current += (config::PAGE_SIZE - offset).min(end - current);
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
            let (ppn, offset) = self.user_page(current, PTEFlags::R)?;
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
            let (ppn, offset) = self.user_page(current, PTEFlags::W)?;
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
        let (ppn, offset) = self.user_page(address, PTEFlags::R | PTEFlags::W)?;
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
            if self.user_page(current, PTEFlags::R).is_err() {
                match self.handle_page_fault_with_limits(current, PageFaultAccess::Read, limits) {
                    Ok(PageFaultOutcome::Handled) => {}
                    Err(MemoryError::OutOfMemory) => return Err(UserAccessError::OutOfMemory),
                    _ => return Err(UserAccessError::Fault),
                }
            }
            let (ppn, offset) = self.user_page(current, PTEFlags::R)?;
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
