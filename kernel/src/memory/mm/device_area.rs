use crate::memory::DeviceBacking;
use alloc::sync::Arc;
use core::ops::Range;

use super::*;

/// @description device-backed VMA partition 对不可回收物理 extent 的共享 view。
#[derive(Debug, Clone)]
pub(super) struct DeviceArea {
    /// backing 释放后仍不复用的共享 futex identity。
    pub(super) identity: u64,
    /// handle、fork descendant 与 VMA partition 共用的 scatter/gather backing owner。
    pub(super) backing: Arc<DeviceBacking>,
    /// 当前 VMA 首页在完整 extent 内的页偏移。
    pub(super) page_offset: usize,
}

impl DeviceArea {
    /// @description 按 VMA split 边界派生三个共享 backing view。
    ///
    /// @param area 原 device view。
    /// @param original_start 原 VMA 首页。
    /// @param start middle partition 首页。
    /// @param end right partition 首页。
    /// @return left/middle/right 对应 view；原 VMA 非 device 时全部为空。
    pub(super) fn partition(
        area: Option<Self>,
        original_start: VirtualPageNumber,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> (Option<Self>, Option<Self>, Option<Self>) {
        let Some(area) = area else {
            return (None, None, None);
        };
        let middle_offset = area.page_offset + start.as_usize() - original_start.as_usize();
        let right_offset = area.page_offset + end.as_usize() - original_start.as_usize();
        (
            Some(area.clone()),
            Some(Self {
                identity: area.identity,
                backing: area.backing.clone(),
                page_offset: middle_offset,
            }),
            Some(Self {
                identity: area.identity,
                backing: area.backing,
                page_offset: right_offset,
            }),
        )
    }
}

impl MapArea {
    /// @description 构造直接映射物理 extent 的 device-backed 用户 VMA。
    ///
    /// @param start_va VMA 起始虚拟地址。
    /// @param end_va VMA exclusive 结束地址。
    /// @param permissions 用户页权限。
    /// @param source DRM 已验证长度和访问权的 backing source。
    /// @return 尚未提交页表的 device MapArea。
    pub(in crate::memory::mm) fn device(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        source: DeviceMappingSource,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::Device;
        area.device = Some(DeviceArea {
            identity: source.identity,
            backing: source.backing,
            page_offset: source.page_offset,
        });
        area
    }

    fn device_ppn(&self, vpn: VirtualPageNumber) -> Result<PhysicalPageNumber, MemoryError> {
        let area = self.device.as_ref().ok_or(MemoryError::InvalidRange)?;
        let index = area
            .page_offset
            .checked_add(vpn.as_usize() - self.vpn_range.start.as_usize())
            .filter(|index| *index < area.backing.pages())
            .ok_or(MemoryError::InvalidRange)?;
        area.backing.page(index).ok_or(MemoryError::InvalidRange)
    }

    /// @description 将 device extent 直接映射到当前页表，不建立第二份 resident index。
    pub(super) fn map_device_area(&self, page_table: &mut PageTable) -> Result<(), MemoryError> {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if Self::has_leaf_permission(self.map_permission) {
                page_table.map(
                    vpn,
                    self.device_ppn(vpn)?,
                    PTEFlags::from_bits(self.map_permission.bits()).unwrap(),
                )?;
            } else {
                page_table.reserve(vpn)?;
            }
        }
        Ok(())
    }

    /// @description 撤销 device VMA 的全部 leaf/reserved slots；backing 由 Arc 独立保活。
    pub(super) fn unmap_device_area(&self, page_table: &mut PageTable) {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            let _ = page_table.unmap(VirtualPageNumber::from_vpn(vpn));
        }
    }

    /// @description 在不改变 backing owner 的前提下切换 device VMA 页权限。
    pub(super) fn protect_device_area(
        &self,
        page_table: &mut PageTable,
        range: Range<VirtualPageNumber>,
        permission: MapPermission,
    ) -> Result<(), MemoryError> {
        let old_leaf = Self::has_leaf_permission(self.map_permission);
        let new_leaf = Self::has_leaf_permission(permission);
        let flags = PTEFlags::from_bits(permission.bits()).unwrap();
        for vpn in range.start.as_usize()..range.end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            match (old_leaf, new_leaf) {
                (true, true) => page_table.set_flags(vpn, flags)?,
                (true, false) => page_table.unmap(vpn)?,
                (false, true) => page_table.map(vpn, self.device_ppn(vpn)?, flags)?,
                (false, false) => {}
            }
        }
        Ok(())
    }

    /// @description fork 时共享同一 device extent 并为 child 建立等价页表。
    pub(super) fn try_clone_device_into(
        &self,
        page_table: &mut PageTable,
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
        if let Err(error) = cloned.map_device_area(page_table) {
            cloned.unmap_device_area(page_table);
            return Err(error);
        }
        Ok(cloned)
    }
}
