//! 设备地址空间：IOMMU 为一个 DMA stream 遍历的页表。
//!
//! 与 CPU 地址空间使用同一架构页表格式（SMMUv3 stage-1 与 AArch64 stage-1 相同，RISC-V IOMMU
//! first-stage 与 Sv39 相同）。CPU 从不激活这张表，因此撤销映射只需设备侧失效：调用者先完成
//! IOMMU 的 TLB 失效，再调用 [`Retired::release`] 释放被摘下的页表页。

use super::page_table::{PagePermissions, PageTable};
use super::translation::TranslationCommit;
use super::VmError;
use crate::arch::mmu::AddressSpaceKind;
use crate::memory::{PhysicalPageNumber, VirtualPageNumber};

/// 一个 DMA stream 的设备页表。
pub(crate) struct DeviceAddressSpace {
    table: PageTable,
}

/// 已撤销、等待设备侧失效完成后才能释放的页表页。
#[must_use = "retired device page-table pages must be released after the IOMMU invalidation"]
pub(crate) struct Retired(TranslationCommit);

impl Retired {
    /// 设备侧失效完成后释放页表页。
    pub(crate) fn release(self) {
        self.0.finish_unpublished();
    }
}

impl DeviceAddressSpace {
    /// 创建空页表。
    ///
    /// # Errors
    /// 根表分配失败返回 `NoMemory`。
    pub(crate) fn create() -> Result<Self, VmError> {
        Ok(Self {
            table: PageTable::try_new(AddressSpaceKind::User).map_err(|_| VmError::NoMemory)?,
        })
    }

    /// 根表物理页号。
    pub(crate) fn root(&self) -> PhysicalPageNumber {
        self.table.root_physical_page()
    }

    /// 把 `pages` 依次映射到从 `first` 开始的设备虚拟页。
    ///
    /// # Errors
    /// 页表页分配失败返回 `NoMemory`，已映射部分全部撤销。
    pub(crate) fn map(&mut self, first: usize, pages: &[PhysicalPageNumber], writable: bool) -> Result<(), VmError> {
        let mut permissions = PagePermissions::READ | PagePermissions::USER;
        if writable {
            permissions |= PagePermissions::WRITE;
        }
        let mut commit = TranslationCommit::new();
        for (index, &page) in pages.iter().enumerate() {
            if self
                .table
                .map(VirtualPageNumber::from_vpn(first + index), page, permissions, &mut commit)
                .is_err()
            {
                for mapped in 0..index {
                    let _ = self.table.unmap(VirtualPageNumber::from_vpn(first + mapped), &mut commit);
                }
                // 这些翻译从未交给设备使用。
                commit.finish_unpublished();
                return Err(VmError::NoMemory);
            }
        }
        commit.finish_unpublished();
        Ok(())
    }

    /// 撤销从 `first` 开始的 `count` 页。
    pub(crate) fn unmap(&mut self, first: usize, count: usize) -> Retired {
        let mut commit = TranslationCommit::new();
        for page in first..first + count {
            let _ = self.table.unmap(VirtualPageNumber::from_vpn(page), &mut commit);
        }
        Retired(commit)
    }
}
