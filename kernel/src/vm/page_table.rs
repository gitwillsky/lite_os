//! Memory-owned frame adapter for the architecture-owned page-table mechanism.

use crate::{
    arch::mmu::{AddressSpaceKind, ArchitecturePageTable, ArchitecturePageTableEntry, TablePage},
    memory::{FrameTracker, PhysicalPageNumber, VirtualPageNumber, frame_allocator},
    vm::translation::{TranslationCommit, TranslationTransition},
};

pub(crate) use crate::arch::mmu::{PagePermissions, PageTableError};

/// 页表页的唯一 owner；fence 前的退休页同样由它保活。
#[derive(Debug)]
pub(in crate::vm) struct TableFrame(FrameTracker);

// OWNER: 存活的页表页数（CPU 与设备页表），随 TableFrame 构造与析构增减；只作内存统计。
static TABLE_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 页表占用的页数。
pub(crate) fn table_pages() -> usize {
    TABLE_PAGES.load(core::sync::atomic::Ordering::Relaxed)
}

impl Drop for TableFrame {
    fn drop(&mut self) {
        TABLE_PAGES.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }
}

impl TablePage for TableFrame {
    fn allocate() -> Option<Self> {
        let frame = frame_allocator::alloc()?;
        TABLE_PAGES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Some(Self(frame))
    }

    fn physical_page(&self) -> usize {
        self.0.ppn.as_usize()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PageTableEntry(ArchitecturePageTableEntry);

impl PageTableEntry {
    pub(super) fn accessed(self) -> bool {
        self.0.accessed()
    }

    pub(crate) fn permissions(self) -> PagePermissions {
        self.0.permissions()
    }

    pub(crate) fn ppn(self) -> PhysicalPageNumber {
        self.0.physical_page().into()
    }
}

pub(crate) struct PageTable(ArchitecturePageTable<TableFrame>);

impl core::fmt::Debug for PageTable {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("PageTable")
    }
}

impl PageTable {
    /// 根入口覆盖的字节数；分区边界由 architecture 机制提供。
    pub(super) const SUBTREE_SIZE: usize = ArchitecturePageTable::<TableFrame>::ROOT_SPAN_BYTES;

    /// 借用源的根子树，不增加 frame owner 或复制叶项。
    ///
    /// # Safety
    /// 源须活过目标硬件视图；修改只经源且受同一 owner 锁保护，撤销 fence 覆盖所有 ASID。
    /// # Errors
    /// 目标冲突或源不是独占子树时返回页表错误；不分配。
    pub(super) unsafe fn share_root_range(
        &mut self,
        target: &mut Self,
        range: core::ops::Range<usize>,
    ) -> Result<(), PageTableError> {
        // SAFETY: 调用者承担上面列出的跨 owner 生命周期与串行化约束。
        unsafe {
            self.0.share_root_range(
                &mut target.0,
                range.start / crate::memory::PAGE_SIZE
                    ..range.end.div_ceil(crate::memory::PAGE_SIZE),
            )
        }
    }

    pub(crate) fn try_new(kind: AddressSpaceKind) -> Result<Self, PageTableError> {
        ArchitecturePageTable::try_new(kind).map(Self)
    }

    /// 根表的物理页号。
    pub(crate) fn root_physical_page(&self) -> PhysicalPageNumber {
        self.0.root_physical_page().into()
    }

    pub(crate) fn token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.0.token()
    }

    pub(crate) fn kernel_trap_token(&self) -> crate::arch::mmu::KernelTrapToken {
        self.0.kernel_trap_token()
    }

    /// 激活显式声明为 kernel 的 architecture root。
    pub(crate) fn activate_kernel(&self) {
        self.0.activate_kernel();
    }

    /// 在全 CPU fence 完成后把 architecture ASID 交还唯一 allocator。
    pub(crate) fn release_address_space_id_after_global_fence(&mut self) {
        self.0.release_address_space_id_after_global_fence();
    }

    pub(in crate::vm) fn map(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        self.0
            .map(usize::from(vpn), usize::from(ppn), permissions)?;
        commit.record(vpn.as_usize(), TranslationTransition::Publish);
        if permissions.contains(PagePermissions::EXECUTE) {
            commit.record_instruction_publication(ppn.as_usize(), 1);
        }
        Ok(())
    }

    pub(in crate::vm) fn map_contiguous_range(
        &mut self,
        virtual_start: VirtualPageNumber,
        physical_start: PhysicalPageNumber,
        page_count: usize,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        let virtual_start = virtual_start.as_usize();
        self.0.map_contiguous_range(
            virtual_start,
            physical_start.as_usize(),
            page_count,
            permissions,
        )?;
        commit.record_range(virtual_start, page_count, TranslationTransition::Publish);
        if permissions.contains(PagePermissions::EXECUTE) {
            commit.record_instruction_publication(physical_start.as_usize(), page_count);
        }
        Ok(())
    }

    pub(in crate::vm) fn unmap(
        &mut self,
        vpn: VirtualPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        // active AVL node 与 frame owner 一起无分配移交给 fence commit；rollback/OOM 路径
        // 不得在 PTE 已撤销后再次申请 retention storage。
        let unmapped = self.0.unmap(usize::from(vpn))?;
        let (first_page, page_count, retired) = unmapped.into_parts();
        commit.retain_table_pages(retired);
        commit.record_range(first_page, page_count, TranslationTransition::Revoke);
        Ok(())
    }

    pub(in crate::vm) fn set_flags(
        &mut self,
        vpn: VirtualPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        let old_entry = self.translate(vpn).ok_or(PageTableError::NotMapped)?;
        let old = old_entry.permissions();
        self.0.set_flags(usize::from(vpn), permissions)?;
        if permissions != old {
            commit.record(
                vpn.as_usize(),
                if permissions.contains(old) {
                    TranslationTransition::Relax
                } else {
                    TranslationTransition::Revoke
                },
            );
            if permissions.contains(PagePermissions::EXECUTE)
                && !old.contains(PagePermissions::EXECUTE)
            {
                commit.record_instruction_publication(old_entry.ppn().as_usize(), 1);
            }
        }
        Ok(())
    }

    /// 软件 access fault 恢复访问位；权限与物理身份保持不变。
    pub(in crate::vm) fn mark_accessed(
        &mut self,
        vpn: VirtualPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        if !self.0.set_accessed(vpn.as_usize(), true)? {
            commit.record(vpn.as_usize(), TranslationTransition::Publish);
        }
        Ok(())
    }

    /// 采集并清除期望物理页的访问位；不把 COW 遮盖页或未映射页算作该页的访问。
    ///
    /// 返回是否观察到访问。clear 后的全 CPU/ASID fence 使各别名的下一次访问重新可观察。
    pub(in crate::vm) fn harvest_accessed(
        &mut self,
        vpn: VirtualPageNumber,
        expected: PhysicalPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<bool, PageTableError> {
        if self.translate(vpn).is_none_or(|entry| {
            entry.ppn() != expected || !entry.permissions().contains(PagePermissions::USER)
        }) {
            return Ok(false);
        }
        let accessed = self.0.set_accessed(vpn.as_usize(), false)?;
        if accessed {
            commit.record(vpn.as_usize(), TranslationTransition::Revoke);
        }
        Ok(accessed)
    }

    /// 查找范围中首个已安装的页，空页表分支整体跳过；不分配或复制页表状态。
    pub(in crate::vm) fn next_mapped(
        &self,
        first: usize,
        end: usize,
    ) -> Option<(usize, PageTableEntry)> {
        self.0
            .next_mapped(first, end)
            .map(|(page, entry)| (page, PageTableEntry(entry)))
    }

    pub(crate) fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.0.translate(usize::from(vpn)).map(PageTableEntry)
    }
}
