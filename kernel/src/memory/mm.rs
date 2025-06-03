use core::ops::Range;

use alloc::vec::{self, Vec};
use bitflags::bitflags;

use super::{address::VirtualPageNumber, page_table::PageTable};

bitflags! {
    pub struct MapPermission: u8 {
        const R = 1 << 0; // 可读
        const W = 1 << 1; // 可写
        const X = 1 << 2; // 可执行
        const U = 1 << 3; // 用户态可访问 (默认仅 内核 态可访问)
    }
}

pub enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

pub struct MemoryArea {
    vpn_range: Range<VirtualPageNumber>,
    map_type: MapType,
    map_permission: MapPermission,
}

impl MemoryArea {
    pub fn new(
        start_vpn: VirtualPageNumber,
        end_vpn: VirtualPageNumber,
        map_type: MapType,
        permissions: MapPermission,
    ) -> Self {
        Self {
            vpn_range: Range {
                start: start_vpn,
                end: end_vpn,
            },
            map_permission: permissions,
            map_type,
        }
    }
}

pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MemoryArea>,
}

impl MemorySet {
    pub fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }

    pub fn active(&self) {
        let satp_val = self.page_table
    }
}
