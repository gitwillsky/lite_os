use crate::memory::DeviceBacking;
use alloc::sync::Arc;
use core::ops::Range;

use super::*;

/// device-backed VMA partition 对不可回收物理 extent 的共享 view。
///
/// 视图存活期间覆盖 backing 的 `[page_offset, page_offset + page_count)`；构造、clone 与 Drop
/// 对称维护 InsertedPages 的逐页 mapcount，因此 fork、拆分与撤销都无需另行记账。
#[derive(Debug)]
pub(super) struct DeviceArea {
    /// backing 释放后仍不复用的共享 futex identity。
    pub(super) identity: u64,
    /// handle、fork descendant 与 VMA partition 共用的 scatter/gather backing owner。
    pub(super) backing: Arc<DeviceBacking>,
    /// 当前 VMA 首页在完整 extent 内的页偏移；backing 至多 16384 页，u32 使 MapArea 不增大。
    pub(super) page_offset: u32,
    /// 与所属 VMA 等长的覆盖页数。
    page_count: u32,
    // 与 backing 独立保留实际 OFD 及可选设备对象；VMA split/fork 共用同一 lease。
    pub(super) owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
}

impl DeviceArea {
    fn new(
        identity: u64,
        backing: Arc<DeviceBacking>,
        pages: Range<usize>,
        owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
    ) -> Self {
        let narrow = |value: usize| u32::try_from(value).expect("device backing exceeds u32 pages");
        backing.map_pages(pages.clone());
        Self {
            identity,
            backing,
            page_offset: narrow(pages.start),
            page_count: narrow(pages.len()),
            owner,
        }
    }

    /// 是否为 `vm_insert_page(s)` 插入的普通页；只有这类设备页计入进程 RSS（Linux MM_FILEPAGES）。
    pub(super) fn inserts_pages(&self) -> bool {
        self.backing.kind() == crate::memory::DeviceMemoryKind::InsertedPages
    }

    fn pages(&self) -> Range<usize> {
        let start = self.page_offset as usize;
        start..start + self.page_count as usize
    }

    /// 按 VMA split 边界派生三个共享 backing view。
    ///
    /// # Parameters
    ///
    /// - `area`: 原 device view。
    /// - `original`: 原 VMA 页区间。
    /// - `start`: middle partition 首页。
    /// - `end`: right partition 首页。
    ///
    /// # Returns
    ///
    /// left/middle/right 对应 view；原 VMA 非 device 时全部为空。新 view 先计入再释放原 view，
    /// 覆盖不变的页不会经历 mapcount 归零。
    pub(super) fn partition(
        area: Option<Self>,
        original: Range<VirtualPageNumber>,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> (Option<Self>, Option<Self>, Option<Self>) {
        let Some(area) = area else {
            return (None, None, None);
        };
        let base = area.page_offset as usize;
        let middle = base + start.as_usize() - original.start.as_usize();
        let right = base + end.as_usize() - original.start.as_usize();
        let total = base + original.end.as_usize() - original.start.as_usize();
        let view = |pages: Range<usize>| {
            Self::new(
                area.identity,
                area.backing.clone(),
                pages,
                area.owner.clone(),
            )
        };
        (
            Some(view(base..middle)),
            Some(view(middle..right)),
            Some(view(right..total)),
        )
    }
}

impl Clone for DeviceArea {
    fn clone(&self) -> Self {
        Self::new(
            self.identity,
            self.backing.clone(),
            self.pages(),
            self.owner.clone(),
        )
    }
}

impl Drop for DeviceArea {
    fn drop(&mut self) {
        self.backing.unmap_pages(self.pages());
    }
}

impl MapArea {
    /// 构造直接映射物理 extent 的 device-backed 用户 VMA。
    ///
    /// # Parameters
    ///
    /// - `start_va`: VMA 起始虚拟地址。
    /// - `end_va`: VMA exclusive 结束地址。
    /// - `permissions`: 用户页权限。
    /// - `source`: DRM 已验证长度和访问权的 backing source。
    ///
    /// # Returns
    ///
    /// 尚未提交页表的 device MapArea。
    pub(in crate::memory::mm) fn device(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        source: DeviceMappingSource,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Device;
        let pages = area.vpn_range.end.as_usize() - area.vpn_range.start.as_usize();
        area.device = Some(DeviceArea::new(
            source.identity,
            source.backing,
            source.page_offset..source.page_offset + pages,
            source.owner,
        ));
        area
    }

    fn device_ppn(&self, vpn: VirtualPageNumber) -> Result<PhysicalPageNumber, MemoryError> {
        let area = self.device.as_ref().ok_or(MemoryError::InvalidRange)?;
        let index = (area.page_offset as usize)
            .checked_add(vpn.as_usize() - self.vpn_range.start.as_usize())
            .filter(|index| *index < area.backing.pages())
            .ok_or(MemoryError::InvalidRange)?;
        area.backing.page(index).ok_or(MemoryError::InvalidRange)
    }

    /// 将 device extent 直接映射到当前页表，不建立第二份 resident index。
    pub(super) fn map_device_area(
        &self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if Self::has_leaf_permission(self.map_permission) {
                page_table.map(
                    vpn,
                    self.device_ppn(vpn)?,
                    self.map_permission.into(),
                    commit,
                )?;
            } else {
                page_table.reserve(vpn)?;
            }
        }
        Ok(())
    }

    /// 撤销 device VMA 的全部 leaf/reserved slots；backing 由 Arc 独立保活。
    pub(super) fn unmap_device_area(
        &self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn), commit);
        }
    }

    /// 在不改变 backing owner 的前提下切换 device VMA 页权限。
    pub(super) fn protect_device_area(
        &self,
        page_table: &mut PageTable,
        range: Range<VirtualPageNumber>,
        permission: MapPermission,
        commit: &mut TranslationCommit,
    ) -> Result<(), MemoryError> {
        let old_leaf = Self::has_leaf_permission(self.map_permission);
        let new_leaf = Self::has_leaf_permission(permission);
        let flags = permission.into();
        for vpn in range.start.as_usize()..range.end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            match (old_leaf, new_leaf) {
                (true, true) => page_table.set_flags(vpn, flags, commit)?,
                (true, false) => page_table.unmap(vpn, commit)?,
                (false, true) => page_table.map(vpn, self.device_ppn(vpn)?, flags, commit)?,
                (false, false) => {}
            }
        }
        Ok(())
    }

    /// fork 时共享同一 device extent 并为 child 建立等价页表。
    pub(super) fn try_clone_device_into(
        &self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) -> Result<Self, MemoryError> {
        let cloned = Self {
            vpn_range: self.vpn_range.clone(),
            data_page_offset: self.data_page_offset,
            data_frames: FallibleMap::new(),
            map_type: self.map_type,
            map_permission: self.map_permission,
            global: self.global,
            kind: self.kind,
            shared_anonymous: None,
            shared_file: None,
            device: self.device.clone(),
            private_file: None,
            lazy_private: false,
        };
        if let Err(error) = cloned.map_device_area(page_table, commit) {
            let mut rollback = TranslationCommit::new();
            cloned.unmap_device_area(page_table, &mut rollback);
            rollback.finish_unpublished();
            return Err(error);
        }
        Ok(cloned)
    }
}
