use core::{arch::asm, error::Error, ops::Range};

use alloc::{boxed::Box, collections::BTreeMap, vec::Vec, string::String};
use bitflags::bitflags;
use riscv::register::satp::{self, Satp};

use crate::memory::{
    address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress},
    frame_allocator::{FrameTracker, alloc},
    page_table::{PTEFlags, PageTableEntry},
    strampoline,
};

use super::config;
use super::{address::VirtualPageNumber, page_table::PageTable};

bitflags! {
    // PTE Flags 的子集
    #[derive(Debug, Clone, Copy)]
    pub struct MapPermission: u8 {
        const R = 1 << 1; // 可读
        const W = 1 << 2; // 可写
        const X = 1 << 3; // 可执行
        const U = 1 << 4; // 用户态可访问 (默认仅 内核 态可访问)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MapType {
    Identical, // PA <-> VA 恒等映射
    Framed,    // 映射到分配的物理页帧
}

#[derive(Debug)]
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
            let src = &data[start..len.min(start + config::PAGE_SIZE)];
            let pte = page_table.translate(current_vpn).unwrap();
            let ppn = pte.ppn();
            let dst = &mut ppn.get_bytes_array_mut()[..src.len()];
            dst.copy_from_slice(src);
            start += config::PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn = current_vpn.next();
        }
    }

    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn));
        }
    }

    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range.start.as_usize()..self.vpn_range.end.as_usize() {
            self.unmap_one(page_table, VirtualPageNumber::from_vpn(vpn));
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
                ppn = vpn.as_usize().into();
            }
        }

        let pte_flags = PTEFlags::from_bits(self.map_permission.bits()).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }

    fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtualPageNumber) {
        match self.map_type {
            MapType::Framed => {
                self.data_frames.remove(&vpn);
            }
            MapType::Identical => {}
        }
        page_table.unmap(vpn);
    }

    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtualPageNumber) {
        for vpn in new_end.as_usize()..self.vpn_range.end.as_usize() {
            self.unmap_one(page_table, VirtualPageNumber::from_vpn(vpn));
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        }
    }

    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtualPageNumber) {
        for vpn in self.vpn_range.end.as_usize()..new_end.as_usize() {
            self.map_one(page_table, VirtualPageNumber::from_vpn(vpn));
        }
        self.vpn_range = Range {
            start: self.vpn_range.start,
            end: new_end,
        }
    }

    pub fn from_another(another: &MapArea) -> Self {
        Self {
            vpn_range: another.vpn_range.clone(),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_permission: another.map_permission,
        }
    }
}

#[derive(Debug)]
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
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }

    pub fn insert_framed_area(
        &mut self,
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }

    pub fn token(&self) -> usize {
        self.page_table.token()
    }

    pub fn map_trampoline(&mut self) {
        let trampoline_va = VirtualAddress::from(config::TRAMPOLINE);
        let strampoline_pa = PhysicalAddress::from(strampoline as usize);

        self.page_table.map(
            trampoline_va.into(),
            strampoline_pa.into(),
            PTEFlags::R | PTEFlags::X,
        );
    }

    pub fn active(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(Satp::from_bits(satp));
            asm!("sfence.vma")
        }
    }

    pub fn get_page_table(&self) -> &PageTable {
        &self.page_table
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    pub fn shrink_to(&mut self, start: VirtualAddress, new_end: VirtualAddress) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.start == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    pub fn append_to(&mut self, start: VirtualAddress, new_end: VirtualAddress) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.start == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtualPageNumber) {
        if let Some((idx, area)) = self
            .areas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.vpn_range.start == start_vpn)
        {
            area.unmap(&mut self.page_table);
            self.areas.remove(idx);
        }
    }

    pub fn from_elf(elf_data: &[u8]) -> Result<(Self, usize, usize), Box<dyn Error>> {
        let mut memory_set = MemorySet::new();

        memory_set.map_trampoline();

        let elf = xmas_elf::ElfFile::new(elf_data)?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf format");
        let ph_count = elf_header.pt2.ph_count();

        let mut max_mapped_vpn = VirtualPageNumber::from(0);

        for i in 0..ph_count {
            let ph = elf.program_header(i)?;
            if ph.get_type()? != xmas_elf::program::Type::Load {
                continue;
            }
            let start_va: VirtualAddress = (ph.virtual_addr() as usize).into();
            let end_va: VirtualAddress = ((ph.virtual_addr() + ph.mem_size()) as usize).into();

            let mut map_perm = MapPermission::U;
            let ph_flags = ph.flags();
            if ph_flags.is_execute() {
                map_perm |= MapPermission::X
            }
            if ph_flags.is_read() {
                map_perm |= MapPermission::R
            }
            if ph_flags.is_write() {
                map_perm |= MapPermission::W
            }
            let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);

            // 记录实际映射的最大页面号
            max_mapped_vpn = max_mapped_vpn
                .as_usize()
                .max(map_area.vpn_range.end.as_usize())
                .into();

            memory_set.push(
                map_area,
                Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
            );
        }

        let max_end_va: VirtualAddress = max_mapped_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += config::PAGE_SIZE;
        let user_stack_top = user_stack_bottom + config::USER_STACK_SIZE;

        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );

        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );

        memory_set.push(
            MapArea::new(
                config::TRAP_CONTEXT.into(),
                config::TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );

        let entry_point = elf.header.pt2.entry_point() as usize;

        Ok((memory_set, user_stack_top, entry_point))
    }

    /// Create a new memory set from ELF data with argument support
    pub fn from_elf_with_args(
        elf_data: &[u8], 
        args: &[String], 
        envs: &[String]
    ) -> Result<(Self, usize, usize), Box<dyn Error>> {
        let mut memory_set = MemorySet::new();

        memory_set.map_trampoline();

        let elf = xmas_elf::ElfFile::new(elf_data)?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf format");
        let ph_count = elf_header.pt2.ph_count();

        let mut max_mapped_vpn = VirtualPageNumber::from(0);

        for i in 0..ph_count {
            let ph = elf.program_header(i)?;
            if ph.get_type()? != xmas_elf::program::Type::Load {
                continue;
            }
            let start_va: VirtualAddress = (ph.virtual_addr() as usize).into();
            let end_va: VirtualAddress = ((ph.virtual_addr() + ph.mem_size()) as usize).into();

            let mut map_perm = MapPermission::U;
            let ph_flags = ph.flags();
            if ph_flags.is_execute() {
                map_perm |= MapPermission::X
            }
            if ph_flags.is_read() {
                map_perm |= MapPermission::R
            }
            if ph_flags.is_write() {
                map_perm |= MapPermission::W
            }
            let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);

            // 记录实际映射的最大页面号
            max_mapped_vpn = max_mapped_vpn
                .as_usize()
                .max(map_area.vpn_range.end.as_usize())
                .into();

            memory_set.push(
                map_area,
                Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
            );
        }

        let max_end_va: VirtualAddress = max_mapped_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += config::PAGE_SIZE;
        let user_stack_top = user_stack_bottom + config::USER_STACK_SIZE;

        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );

        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );

        memory_set.push(
            MapArea::new(
                config::TRAP_CONTEXT.into(),
                config::TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );

        let entry_point = elf.header.pt2.entry_point() as usize;

        // Build argc/argv/envp on the stack
        let actual_stack_top = memory_set.build_arg_stack(user_stack_top, args, envs)?;

        Ok((memory_set, actual_stack_top, entry_point))
    }

    /// Build argc/argv/envp layout on user stack
    fn build_arg_stack(
        &self, 
        stack_top: usize, 
        args: &[String], 
        envs: &[String]
    ) -> Result<usize, Box<dyn Error>> {
        let mut stack_ptr = stack_top;
        
        // Calculate total space needed for strings
        let mut total_string_size = 0;
        for arg in args {
            total_string_size += arg.len() + 1; // +1 for null terminator
        }
        for env in envs {
            total_string_size += env.len() + 1; // +1 for null terminator
        }
        
        // Align to 8 bytes boundary for arguments
        total_string_size = (total_string_size + 7) & !7;
        
        // Space for pointers: argc + argv[] + envp[] + padding
        let argc = args.len();
        let pointer_space = core::mem::size_of::<usize>() * (1 + argc + 1 + envs.len() + 1);
        let pointer_space_aligned = (pointer_space + 7) & !7;
        
        // Move stack pointer down to accommodate everything
        stack_ptr -= total_string_size + pointer_space_aligned;
        stack_ptr &= !7; // Align to 8 bytes
        
        let string_area_start = stack_ptr + pointer_space_aligned;
        let mut string_ptr = string_area_start;
        let mut argv_ptrs = Vec::new();
        let mut envp_ptrs = Vec::new();
        
        // Write argument strings and collect pointers
        for arg in args {
            argv_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, arg)?;
            string_ptr += arg.len() + 1;
        }
        
        // Write environment strings and collect pointers
        for env in envs {
            envp_ptrs.push(string_ptr);
            self.write_string_to_user_stack(string_ptr, env)?;
            string_ptr += env.len() + 1;
        }
        
        // Write argc/argv/envp structure
        let mut ptr_writer = stack_ptr;
        
        // Write argc
        self.write_usize_to_user_stack(ptr_writer, argc)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // Write argv pointers
        for &arg_ptr in &argv_ptrs {
            self.write_usize_to_user_stack(ptr_writer, arg_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        // Null terminator for argv
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        ptr_writer += core::mem::size_of::<usize>();
        
        // Write envp pointers
        for &env_ptr in &envp_ptrs {
            self.write_usize_to_user_stack(ptr_writer, env_ptr)?;
            ptr_writer += core::mem::size_of::<usize>();
        }
        // Null terminator for envp
        self.write_usize_to_user_stack(ptr_writer, 0)?;
        
        Ok(stack_ptr)
    }
    
    /// Write a string to user stack memory
    fn write_string_to_user_stack(&self, addr: usize, s: &str) -> Result<(), Box<dyn Error>> {
        let vpn_start = VirtualAddress::from(addr).floor();
        let vpn_end = VirtualAddress::from(addr + s.len() + 1).floor();
        
        for vpn in vpn_start.as_usize()..=vpn_end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Some(pte) = self.translate(vpn) {
                let ppn = pte.ppn();
                let page_bytes = ppn.get_bytes_array_mut();
                
                let page_start = vpn.as_usize() * config::PAGE_SIZE;
                let page_end = page_start + config::PAGE_SIZE;
                
                let str_start = addr.max(page_start);
                let str_end = (addr + s.len()).min(page_end);
                
                if str_start < str_end {
                    let page_offset = str_start - page_start;
                    let str_offset = str_start - addr;
                    let copy_len = str_end - str_start;
                    
                    page_bytes[page_offset..page_offset + copy_len]
                        .copy_from_slice(&s.as_bytes()[str_offset..str_offset + copy_len]);
                }
                
                // Write null terminator if this page contains the end
                if addr + s.len() >= page_start && addr + s.len() < page_end {
                    let null_offset = (addr + s.len()) - page_start;
                    page_bytes[null_offset] = 0;
                }
            }
        }
        Ok(())
    }
    
    /// Write a usize value to user stack memory
    fn write_usize_to_user_stack(&self, addr: usize, value: usize) -> Result<(), Box<dyn Error>> {
        let bytes = value.to_le_bytes();
        let vpn_start = VirtualAddress::from(addr).floor();
        let vpn_end = VirtualAddress::from(addr + core::mem::size_of::<usize>() - 1).floor();
        
        for vpn in vpn_start.as_usize()..=vpn_end.as_usize() {
            let vpn = VirtualPageNumber::from_vpn(vpn);
            if let Some(pte) = self.translate(vpn) {
                let ppn = pte.ppn();
                let page_bytes = ppn.get_bytes_array_mut();
                
                let page_start = vpn.as_usize() * config::PAGE_SIZE;
                let page_end = page_start + config::PAGE_SIZE;
                
                let val_start = addr.max(page_start);
                let val_end = (addr + core::mem::size_of::<usize>()).min(page_end);
                
                if val_start < val_end {
                    let page_offset = val_start - page_start;
                    let val_offset = val_start - addr;
                    let copy_len = val_end - val_start;
                    
                    page_bytes[page_offset..page_offset + copy_len]
                        .copy_from_slice(&bytes[val_offset..val_offset + copy_len]);
                }
            }
        }
        Ok(())
    }

    pub fn form_existed_user(user_space: &MemorySet) -> Self {
        let mut memory_set = MemorySet::new();
        memory_set.map_trampoline();

        for area in user_space.areas.iter() {
            let new_area = MapArea::from_another(area);
            memory_set.push(new_area, None);

            for vpn in area.vpn_range.start.as_usize()..area.vpn_range.end.as_usize() {
                let vpn = VirtualPageNumber::from_vpn(vpn);
                let src_ppn = user_space.translate(vpn).unwrap().ppn();
                let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                dst_ppn
                    .get_bytes_array_mut()
                    .copy_from_slice(&src_ppn.get_bytes_array_mut());
            }
        }
        memory_set
    }

    pub fn recycle_data_pages(&mut self) {
        self.areas.clear();
    }
}
