use core::{arch::asm, ops::Range};

use alloc::{
    collections::BTreeMap,
    vec::{self, Vec},
};
use bitflags::bitflags;

use crate::memory::{
    address::{PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::PTEFlags,
};

use super::config;
use super::{address::VirtualPageNumber, page_table::PageTable};

bitflags! {
    // PTE Flags 的子集
    pub struct MapPermission: u8 {
        const R = 1 << 1; // 可读
        const W = 1 << 2; // 可写
        const X = 1 << 3; // 可执行
        const U = 1 << 4; // 用户态可访问 (默认仅 内核 态可访问)
    }
}

pub enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

pub struct MapArea {
    vpn_range: Range<VirtualPageNumber>,
    data_frames: BTreeMap<VirtualPageNumber, FrameTracker>,
    map_type: MapType,
    map_permission: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        map_type: MapType,
        permissions: MapPermission,
    ) -> Self {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        Self {
            vpn_range: Range {
                start: start_vpn,
                end: end_vpn,
            },
            data_frames: BTreeMap::new(),
            map_permission: permissions,
            map_type,
        }
    }

    pub fn copy_data(&mut self, page_table: &PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.start;
        let len = data.len();

        loop {
            let src = &data[start..len.min(star + config::PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array_mut()[..src.len()];
            dst.copy_from_slice(src);
            start += config::PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn += 1;
        }
    }

    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }

    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }

    fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtualPageNumber) {
        let ppn: PhysicalPageNumber;
        match self.map_type {
            MapType::Framed => {
                let frame = alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
            MapType::Identical => {
                ppn = vpn.into();
            }
        }

        let pte_flags = PTEFlags::from_bits(self.map_permission.bits()).unwrap();
        page_table.map(vpn, ppn, flags);
    }

    fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtualPageNumber) {
        match self.map_type {
            MapType::Framed => self.data_frames.remove(&vpn),
            _ => {}
        }
        page_table.unmap(vpn);
    }
}

pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
}

impl MemorySet {
    pub fn new() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
        }
    }

    pub fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&self.page_table, data);
        }
        self.areas.push(map_area);
    }

    pub fn insert_framed_area(
        &mut self,
        start_va: VirtualPageNumber,
        end_va: VirtualPageNumber,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Identical, permission),
            None,
        );
    }
}
